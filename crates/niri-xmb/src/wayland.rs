//! Multi-output wlr-layer-shell surfaces for the niri-xmb bar and popup.

use crate::config::{Align, BarConfig, Edge};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        Capability, SeatHandler, SeatState,
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RepeatInfo},
        pointer::{
            CursorIcon, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec, ThemedPointer,
        },
    },
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler,
            LayerSurface as SctLayerSurface, LayerSurfaceConfigure,
        },
    },
    shm::{
        Shm, ShmHandler,
        slot::{Buffer, SlotPool},
    },
};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::{Duration, Instant};
use wayland_client::{
    Connection, EventQueue, Proxy, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_region, wl_seat, wl_shm, wl_surface},
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceKind {
    Bar(usize),
    Popup,
}

#[derive(Debug, Clone)]
pub struct SurfaceInfo {
    pub id: usize,
    pub kind: SurfaceKind,
    pub output: String,
    pub global_x: f64,
    pub global_y: f64,
    pub width: u32,
    pub height: u32,
    pub configured: bool,
    pub frame_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    fn right(self) -> u32 {
        self.x + self.width
    }

    fn bottom(self) -> u32 {
        self.y + self.height
    }

    pub fn union(self, other: Self) -> Self {
        let x = self.x.min(other.x);
        let y = self.y.min(other.y);
        let right = self.right().max(other.right());
        let bottom = self.bottom().max(other.bottom());
        Self {
            x,
            y,
            width: right - x,
            height: bottom - y,
        }
    }
}

struct SurfaceBuffer {
    buffer: Buffer,
    width: u32,
    height: u32,
    content: Option<DamageRect>,
}

#[derive(Debug, Clone)]
pub enum UiEventKind {
    Enter,
    Leave,
    Motion,
    Press {
        button: u32,
    },
    Release {
        button: u32,
    },
    /// One complete wheel step. High-resolution packet values are accumulated
    /// before this event reaches the UI.
    Scroll {
        steps: i32,
    },
    Key {
        keysym: u32,
    },
}

/// Convert high-resolution Wayland wheel data into complete wheel steps.
///
/// A G502X ratchet click can arrive as `30 + 6 * 15` value120 units. Do not
/// act on a partial value, because that makes one physical click move several
/// workspace rows. Free-spin still emits completed 120-unit steps rapidly.
#[derive(Debug, Default)]
struct WheelStepAccumulator {
    value120_remainder: i32,
}

impl WheelStepAccumulator {
    fn steps(&mut self, value120: i32, discrete: i32, absolute: f64) -> i32 {
        if value120 != 0 {
            self.value120_remainder += value120;
            if self.value120_remainder >= 120 {
                self.value120_remainder -= 120;
                1
            } else if self.value120_remainder <= -120 {
                self.value120_remainder += 120;
                -1
            } else {
                0
            }
        } else if discrete != 0 {
            discrete.signum()
        } else if absolute.abs() > f64::EPSILON {
            absolute.signum() as i32
        } else {
            0
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiEvent {
    pub surface_id: usize,
    pub kind: SurfaceKind,
    pub output: String,
    pub x: f64,
    pub y: f64,
    pub global_x: f64,
    pub global_y: f64,
    pub event: UiEventKind,
}

#[derive(Clone)]
pub struct WakeHandle {
    writer: Arc<UnixStream>,
}

impl WakeHandle {
    pub fn wake(&self) {
        let mut writer = &*self.writer;
        match writer.write(&[1]) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(error) => tracing::debug!(%error, "event-loop wake failed"),
        }
    }
}

pub struct WakePipe {
    reader: UnixStream,
    handle: WakeHandle,
}

impl WakePipe {
    pub fn new() -> Result<Self> {
        let (reader, writer) =
            UnixStream::pair().context("failed to create event-loop wake pipe")?;
        reader
            .set_nonblocking(true)
            .context("failed to configure wake reader")?;
        writer
            .set_nonblocking(true)
            .context("failed to configure wake writer")?;
        Ok(Self {
            reader,
            handle: WakeHandle {
                writer: Arc::new(writer),
            },
        })
    }

    pub fn handle(&self) -> WakeHandle {
        self.handle.clone()
    }

    fn drain(&self) {
        let mut reader = &self.reader;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok() {}
    }
}

#[derive(Debug, Clone)]
enum SurfaceSpecKind {
    Bar { index: usize, config: BarConfig },
    Popup,
}

#[derive(Debug, Clone)]
struct SurfaceSpec {
    output: String,
    kind: SurfaceSpecKind,
}

/// Keep one dormant popup surface specification for every known output.
///
/// Virtual outputs can appear after the daemon starts. The specification must
/// be added when Wayland reports the output so `sync_surfaces` can create the
/// layer surface without a daemon restart.
fn ensure_popup_spec(specs: &mut Vec<SurfaceSpec>, output: &str) -> bool {
    if specs
        .iter()
        .any(|spec| spec.output == output && matches!(&spec.kind, SurfaceSpecKind::Popup))
    {
        return false;
    }
    specs.push(SurfaceSpec {
        output: output.to_string(),
        kind: SurfaceSpecKind::Popup,
    });
    true
}

struct OutputRecord {
    output: wl_output::WlOutput,
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// Which input classes a popup surface accepts.
///
/// Pointer and keyboard are separate because the peek overlay wants exactly one
/// of them: it may follow the pointer so the panel can be grabbed by hand, while
/// leaving the keyboard alone so the Niri keybinds that raised it keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PopupInput {
    pub pointer: bool,
    pub keyboard: bool,
}

impl PopupInput {
    /// A passive readout: clicks and keys pass straight through.
    pub const NONE: Self = Self {
        pointer: false,
        keyboard: false,
    };
    /// Grabbable by pointer, but still transparent to the keyboard.
    pub const POINTER: Self = Self {
        pointer: true,
        keyboard: false,
    };
    /// A session the user drives.
    pub const FULL: Self = Self {
        pointer: true,
        keyboard: true,
    };
}

struct SurfaceEntry {
    id: usize,
    kind: SurfaceKind,
    output: String,
    output_x: i32,
    output_y: i32,
    output_width: i32,
    output_height: i32,
    layer_surface: Option<SctLayerSurface>,
    pool: SlotPool,
    width: u32,
    height: u32,
    configured: bool,
    visible: bool,
    input: PopupInput,
    capture_outside: bool,
    input_rect: Option<DamageRect>,
    frame_ready: bool,
    buffers: Vec<SurfaceBuffer>,
    last_damage: Option<DamageRect>,
}

pub struct SurfaceManager {
    queue: EventQueue<WaylandState>,
    state: WaylandState,
}

impl SurfaceManager {
    pub fn new(
        bars: &[BarConfig],
        popup_outputs: &[String],
        event_tx: Sender<UiEvent>,
    ) -> Result<Self> {
        let connection = Connection::connect_to_env().context("failed to connect to Wayland")?;
        let (globals, queue) = registry_queue_init::<WaylandState>(&connection)
            .context("failed to initialize Wayland registry")?;
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor missing")?;
        let layer_shell = LayerShell::bind(&globals, &qh).context("wlr-layer-shell missing")?;
        let shm = Shm::bind(&globals, &qh).context("wl_shm missing")?;
        let seat_state = SeatState::new(&globals, &qh);
        let virtual_pointer_manager = globals
            .bind::<ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ())
            .ok();
        let virtual_pointer = virtual_pointer_manager
            .as_ref()
            .map(|manager| manager.create_virtual_pointer(None, &qh, ()));

        let mut specs = Vec::new();
        for (index, config) in bars.iter().cloned().enumerate() {
            specs.push(SurfaceSpec {
                output: config.output.clone(),
                kind: SurfaceSpecKind::Bar { index, config },
            });
        }
        for output in popup_outputs {
            ensure_popup_spec(&mut specs, output);
        }

        let state = WaylandState {
            registry: RegistryState::new(&globals),
            compositor,
            output_state: OutputState::new(&globals, &qh),
            seat_state,
            shm,
            layer_shell,
            specs,
            outputs: HashMap::new(),
            surfaces: Vec::new(),
            next_surface_id: 1,
            pointer: None,
            keyboard: None,
            event_tx,
            pointer_x: 0.0,
            pointer_y: 0.0,
            pointer_surface: None,
            wheel_steps: WheelStepAccumulator::default(),
            keyboard_surface: None,
            _virtual_pointer_manager: virtual_pointer_manager,
            virtual_pointer,
            started_at: Instant::now(),
        };

        let mut manager = Self { queue, state };
        manager.queue.roundtrip(&mut manager.state)?;
        manager.queue.roundtrip(&mut manager.state)?;
        manager.state.sync_surfaces(&manager.queue.handle());
        manager.queue.roundtrip(&mut manager.state)?;
        manager.prime_popup_buffers()?;
        manager.hide_all_popups()?;
        Ok(manager)
    }

    /// Allocate the two buffers needed by compositor frame pacing before the
    /// first hotkey press. The SHM pages remain untouched until rendered, so
    /// this reserves buffer objects without eagerly faulting full outputs into
    /// resident memory.
    fn prime_popup_buffers(&mut self) -> Result<()> {
        for surface in &mut self.state.surfaces {
            if surface.kind != SurfaceKind::Popup || !surface.configured {
                continue;
            }
            while surface.buffers.len() < 2 {
                allocate_surface_buffer(surface)?;
            }
        }
        Ok(())
    }

    pub fn dispatch(&mut self, wake: &WakePipe, timeout: Duration) -> Result<()> {
        if let Err(error) = self.queue.flush() {
            tracing::debug!(%error, "Wayland flush failed");
        }
        if let Some(guard) = self.queue.prepare_read() {
            use rustix::event::{PollFd, PollFlags, poll};
            use rustix::fd::{AsFd, BorrowedFd};
            use rustix::time::Timespec;
            use std::os::fd::AsRawFd;
            let raw = self.queue.as_fd().as_raw_fd();
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
            let mut fds = [
                PollFd::new(&borrowed, PollFlags::IN),
                PollFd::new(&wake.reader, PollFlags::IN),
            ];
            let timeout = Timespec {
                tv_sec: timeout.as_secs().try_into().unwrap_or(i64::MAX),
                tv_nsec: timeout.subsec_nanos().into(),
            };
            let _ = poll(&mut fds, Some(&timeout));
            if fds[0].revents().contains(PollFlags::IN) {
                let _ = guard.read();
            } else {
                drop(guard);
            }
            if fds[1].revents().contains(PollFlags::IN) {
                wake.drain();
            }
        }
        self.queue.dispatch_pending(&mut self.state)?;
        self.state.sync_surfaces(&self.queue.handle());
        Ok(())
    }

    pub fn surfaces(&self) -> Vec<SurfaceInfo> {
        self.state
            .surfaces
            .iter()
            .filter(|surface| surface.kind != SurfaceKind::Popup || surface.visible)
            .map(|surface| {
                let origin = surface_origin(surface, &self.state.specs);
                SurfaceInfo {
                    id: surface.id,
                    kind: surface.kind,
                    output: surface.output.clone(),
                    global_x: surface.output_x as f64 + origin.0,
                    global_y: surface.output_y as f64 + origin.1,
                    width: surface.width,
                    height: surface.height,
                    configured: surface.configured,
                    frame_ready: surface.frame_ready,
                }
            })
            .collect()
    }

    pub fn present(
        &mut self,
        surface_id: usize,
        pixels: &[u8],
        current_damage: Option<DamageRect>,
    ) -> Result<bool> {
        let Some(surface) = self
            .state
            .surfaces
            .iter_mut()
            .find(|surface| surface.id == surface_id)
        else {
            return Ok(false);
        };
        if !surface.visible
            || !surface.configured
            || !surface.frame_ready
            || surface.width == 0
            || surface.height == 0
        {
            return Ok(false);
        }

        let display_damage = match (surface.last_damage, current_damage) {
            (Some(previous), Some(current)) => previous.union(current),
            (Some(previous), None) => previous,
            (None, Some(current)) => current,
            (None, None) => return Ok(false),
        };

        let mut available = None;
        let mut index = 0;
        while index < surface.buffers.len() {
            let size_matches = surface.buffers[index].width == surface.width
                && surface.buffers[index].height == surface.height;
            let released = surface
                .pool
                .canvas(&surface.buffers[index].buffer)
                .is_some();
            if size_matches && released {
                available = Some(index);
                break;
            }
            if !size_matches && released {
                surface.buffers.swap_remove(index);
                continue;
            }
            index += 1;
        }

        let (buffer_index, submitted_damage) = if let Some(index) = available {
            let submitted_damage = surface.buffers[index]
                .content
                .map_or(display_damage, |content| content.union(display_damage));
            let canvas = surface
                .pool
                .canvas(&surface.buffers[index].buffer)
                .context("reusable SHM buffer became unavailable")?;
            copy_rgba_to_argb8888_region(canvas, pixels, surface.width, submitted_damage);
            surface.buffers[index].content = current_damage;
            (index, submitted_damage)
        } else {
            allocate_surface_buffer(surface)?;
            let index = surface.buffers.len() - 1;
            let canvas = surface
                .pool
                .canvas(&surface.buffers[index].buffer)
                .context("new SHM buffer became unavailable")?;
            copy_rgba_to_argb8888_region(canvas, pixels, surface.width, display_damage);
            surface.buffers[index].content = current_damage;
            (index, display_damage)
        };

        let Some(layer_surface) = &surface.layer_surface else {
            return Ok(false);
        };
        let wl_surface = layer_surface.wl_surface();
        wl_surface.frame(&self.queue.handle(), wl_surface.clone());
        surface.buffers[buffer_index]
            .buffer
            .attach_to(wl_surface)
            .context("failed to attach SHM buffer")?;
        wl_surface.damage_buffer(
            submitted_damage.x as i32,
            submitted_damage.y as i32,
            submitted_damage.width as i32,
            submitted_damage.height as i32,
        );
        surface.last_damage = current_damage;
        surface.frame_ready = false;
        layer_surface.commit();
        Ok(true)
    }

    pub fn show_popups(&mut self, outputs: &[String], capture_outside: bool) -> Result<()> {
        self.show_popups_with(outputs, PopupInput::FULL, capture_outside)
    }

    /// Show the popup on `outputs`.
    ///
    /// `input` decides what the surface accepts. `PopupInput::NONE` is what
    /// makes the peek overlay a passive readout, letting the compositor
    /// keybinds the user is pressing continue to reach Niri and clicks pass
    /// through to whatever is underneath.
    pub fn show_popups_with(
        &mut self,
        outputs: &[String],
        input: PopupInput,
        capture_outside: bool,
    ) -> Result<()> {
        for index in 0..self.state.surfaces.len() {
            if self.state.surfaces[index].kind != SurfaceKind::Popup {
                continue;
            }
            let show = outputs
                .iter()
                .any(|output| output == &self.state.surfaces[index].output);
            if show {
                let remap = self.state.surfaces[index].layer_surface.is_none();
                if remap {
                    self.state.map_popup_surface(index, &self.queue.handle())?;
                }
                let surface = &mut self.state.surfaces[index];
                surface.visible = true;
                surface.input = input;
                surface.capture_outside = capture_outside;
                if remap {
                    // A new layer role needs a fresh configure before the
                    // cached SHM buffer can be attached.
                    surface.configured = false;
                    surface.frame_ready = true;
                    surface.last_damage = None;
                }
                apply_input_region(&self.queue.handle(), &self.state.compositor, surface);
                let Some(layer_surface) = &surface.layer_surface else {
                    continue;
                };
                layer_surface.set_keyboard_interactivity(if input.keyboard {
                    KeyboardInteractivity::Exclusive
                } else {
                    KeyboardInteractivity::None
                });
                layer_surface.commit();
            } else if self.state.surfaces[index].visible
                || self.state.surfaces[index].configured
                || self.state.surfaces[index].layer_surface.is_some()
            {
                let surface = &mut self.state.surfaces[index];
                surface.visible = false;
                surface.input = PopupInput::NONE;
                surface.capture_outside = false;
                surface.input_rect = None;
                surface.configured = false;
                surface.frame_ready = true;
                surface.last_damage = None;
                if self.state.pointer_surface == Some(surface.id) {
                    self.state.pointer_surface = None;
                }
                if self.state.keyboard_surface == Some(surface.id) {
                    self.state.keyboard_surface = None;
                }
                // Dropping the layer role also destroys its wl_surface. Keep
                // the SHM pool and buffers in SurfaceEntry for a cheap remap.
                surface.layer_surface.take();
            }
        }
        // Keep the daemon and buffer pools alive, but destroy hidden popup
        // layer objects. Remapping is asynchronous; the event loop renders
        // after the compositor sends a fresh configure.
        self.queue
            .flush()
            .context("failed to flush popup state to Wayland")?;
        Ok(())
    }

    /// Change what visible popups accept without re-showing them.
    ///
    /// Promotion is the whole point: the surface, its buffers, and its open
    /// animation stay exactly as they are, so handing a peek overlay over to
    /// the pointer or keyboard costs an input region and a commit rather than a
    /// visible close and reopen.
    pub fn set_popup_input(&mut self, input: PopupInput, capture_outside: bool) -> Result<()> {
        let mut changed = false;
        for surface in &mut self.state.surfaces {
            if surface.kind != SurfaceKind::Popup || !surface.visible {
                continue;
            }
            if surface.input == input && surface.capture_outside == capture_outside {
                continue;
            }
            surface.input = input;
            surface.capture_outside = capture_outside;
            apply_input_region(&self.queue.handle(), &self.state.compositor, surface);
            let Some(layer_surface) = &surface.layer_surface else {
                continue;
            };
            layer_surface.set_keyboard_interactivity(if input.keyboard {
                KeyboardInteractivity::Exclusive
            } else {
                KeyboardInteractivity::None
            });
            layer_surface.commit();
            changed = true;
        }
        if changed {
            self.queue
                .flush()
                .context("failed to flush popup input change")?;
        }
        Ok(())
    }

    pub fn set_popup_input_rect(&mut self, surface_id: usize, rect: DamageRect) -> Result<()> {
        let Some(surface) = self
            .state
            .surfaces
            .iter_mut()
            .find(|surface| surface.id == surface_id && surface.kind == SurfaceKind::Popup)
        else {
            return Ok(());
        };
        if surface.input_rect == Some(rect) {
            return Ok(());
        }
        surface.input_rect = Some(rect);
        if surface.visible && surface.input.pointer && !surface.capture_outside {
            apply_input_region(&self.queue.handle(), &self.state.compositor, surface);
            if let Some(layer_surface) = &surface.layer_surface {
                layer_surface.commit();
            }
            self.queue
                .flush()
                .context("failed to flush popup panel input region")?;
        }
        Ok(())
    }

    pub fn hide_all_popups(&mut self) -> Result<()> {
        self.show_popups_with(&[], PopupInput::NONE, false)
    }

    pub fn cursor_restore_available(&self) -> bool {
        self.state.virtual_pointer.is_some()
    }

    /// Send one complete click through the virtual pointer at a global point.
    ///
    /// The caller must hide its input surface first. Wayland processes these
    /// requests in order, so the click reaches the surface below the popup.
    pub fn replay_pointer_click(&mut self, global_x: f64, global_y: f64, button: u32) -> bool {
        let Some(pointer) = &self.state.virtual_pointer else {
            return false;
        };
        let Some(min_x) = self.state.outputs.values().map(|output| output.x).min() else {
            return false;
        };
        let Some(min_y) = self.state.outputs.values().map(|output| output.y).min() else {
            return false;
        };
        let Some(max_x) = self
            .state
            .outputs
            .values()
            .map(|output| output.x + output.width)
            .max()
        else {
            return false;
        };
        let Some(max_y) = self
            .state
            .outputs
            .values()
            .map(|output| output.y + output.height)
            .max()
        else {
            return false;
        };
        let width = (max_x - min_x).max(1) as u32;
        let height = (max_y - min_y).max(1) as u32;
        let x = (global_x - min_x as f64).round().clamp(0.0, width as f64) as u32;
        let y = (global_y - min_y as f64).round().clamp(0.0, height as f64) as u32;
        let time = self
            .state
            .started_at
            .elapsed()
            .as_millis()
            .min(u32::MAX as u128) as u32;

        pointer.motion_absolute(time, x, y, width, height);
        pointer.button(time, button, wl_pointer::ButtonState::Pressed);
        pointer.frame();
        pointer.button(
            time.saturating_add(1),
            button,
            wl_pointer::ButtonState::Released,
        );
        pointer.frame();
        self.queue.flush().is_ok()
    }

    pub fn restore_pointer(&mut self, global_x: f64, global_y: f64) -> bool {
        let Some(pointer) = &self.state.virtual_pointer else {
            return false;
        };
        let Some(min_x) = self.state.outputs.values().map(|output| output.x).min() else {
            return false;
        };
        let Some(min_y) = self.state.outputs.values().map(|output| output.y).min() else {
            return false;
        };
        let Some(max_x) = self
            .state
            .outputs
            .values()
            .map(|output| output.x + output.width)
            .max()
        else {
            return false;
        };
        let Some(max_y) = self
            .state
            .outputs
            .values()
            .map(|output| output.y + output.height)
            .max()
        else {
            return false;
        };
        let width = (max_x - min_x).max(1) as u32;
        let height = (max_y - min_y).max(1) as u32;
        let x = (global_x - min_x as f64).round().clamp(0.0, width as f64) as u32;
        let y = (global_y - min_y as f64).round().clamp(0.0, height as f64) as u32;
        let time = self
            .state
            .started_at
            .elapsed()
            .as_millis()
            .min(u32::MAX as u128) as u32;
        pointer.motion_absolute(time, x, y, width, height);
        pointer.frame();
        self.queue.flush().is_ok()
    }
}

fn allocate_surface_buffer(surface: &mut SurfaceEntry) -> Result<()> {
    let stride = surface.width * 4;
    let (buffer, _) = surface
        .pool
        .create_buffer(
            surface.width as i32,
            surface.height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
        )
        .context("failed to create SHM buffer")?;
    surface.buffers.push(SurfaceBuffer {
        buffer,
        width: surface.width,
        height: surface.height,
        content: None,
    });
    Ok(())
}

/// Find the nontransparent bounds of a premultiplied RGBA frame.
#[cfg(test)]
fn alpha_bounds(source: &[u8], width: u32, height: u32) -> Option<DamageRect> {
    let mut min_x = width;
    let mut min_y = height;
    let mut max_x = 0;
    let mut max_y = 0;
    let mut found = false;

    for y in 0..height {
        let row = y as usize * width as usize * 4;
        for x in 0..width {
            let alpha = source.get(row + x as usize * 4 + 3).copied().unwrap_or(0);
            if alpha == 0 {
                continue;
            }
            found = true;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x + 1);
            max_y = max_y.max(y + 1);
        }
    }

    found.then_some(DamageRect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

/// tiny-skia exposes premultiplied RGBA bytes. Wayland ARGB8888 is BGRA in
/// little-endian memory, so red and blue swap only inside the damaged rows.
fn copy_rgba_to_argb8888_region(
    destination: &mut [u8],
    source: &[u8],
    surface_width: u32,
    damage: DamageRect,
) {
    let stride = surface_width as usize * 4;
    let x0 = damage.x as usize * 4;
    let x1 = damage.right() as usize * 4;
    for y in damage.y as usize..damage.bottom() as usize {
        let start = y * stride + x0;
        let end = y * stride + x1;
        let Some(dst_row) = destination.get_mut(start..end) else {
            continue;
        };
        let Some(src_row) = source.get(start..end) else {
            continue;
        };
        for (dst, src) in dst_row.chunks_exact_mut(4).zip(src_row.chunks_exact(4)) {
            dst[0] = src[2];
            dst[1] = src[1];
            dst[2] = src[0];
            dst[3] = src[3];
        }
    }
}

#[cfg(test)]
fn copy_rgba_to_argb8888(destination: &mut [u8], source: &[u8]) {
    let pixels = (source.len() / 4) as u32;
    copy_rgba_to_argb8888_region(
        destination,
        source,
        pixels,
        DamageRect {
            x: 0,
            y: 0,
            width: pixels,
            height: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{
        DamageRect, SurfaceSpecKind, WheelStepAccumulator, alpha_bounds, copy_rgba_to_argb8888,
        copy_rgba_to_argb8888_region, ensure_popup_spec,
    };

    #[test]
    fn converts_tiny_skia_rgba_to_wayland_argb8888_memory_order() {
        let source = [0x11, 0x22, 0x33, 0x44, 0xaa, 0xbb, 0xcc, 0xdd];
        let mut destination = [0; 8];
        copy_rgba_to_argb8888(&mut destination, &source);
        assert_eq!(
            destination,
            [0x33, 0x22, 0x11, 0x44, 0xcc, 0xbb, 0xaa, 0xdd]
        );
    }

    #[test]
    fn finds_only_nontransparent_pixel_bounds() {
        let mut source = vec![0; 3 * 2 * 4];
        source[7] = 0xff;
        source[23] = 0x40;

        assert_eq!(
            alpha_bounds(&source, 3, 2),
            Some(DamageRect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            })
        );
    }

    #[test]
    fn regional_conversion_leaves_undamaged_pixels_untouched() {
        let source = [1, 2, 3, 4, 10, 20, 30, 40, 5, 6, 7, 8, 50, 60, 70, 80];
        let mut destination = [0xaa; 16];
        copy_rgba_to_argb8888_region(
            &mut destination,
            &source,
            2,
            DamageRect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            },
        );

        assert_eq!(&destination[..4], &[0xaa; 4]);
        assert_eq!(&destination[4..8], &[30, 20, 10, 40]);
        assert_eq!(&destination[8..], &[0xaa; 8]);
    }

    #[test]
    fn popup_spec_is_added_once_for_hotplugged_output() {
        let mut specs = Vec::new();

        assert!(ensure_popup_spec(&mut specs, "Virtual-1"));
        assert!(!ensure_popup_spec(&mut specs, "Virtual-1"));
        assert!(ensure_popup_spec(&mut specs, "HDMI-A-1"));

        let popup_outputs = specs
            .iter()
            .filter(|spec| matches!(&spec.kind, SurfaceSpecKind::Popup))
            .map(|spec| spec.output.as_str())
            .collect::<Vec<_>>();
        assert_eq!(popup_outputs, ["Virtual-1", "HDMI-A-1"]);
    }

    #[test]
    fn high_resolution_wheel_waits_for_a_full_value120_step() {
        let mut wheel = WheelStepAccumulator::default();
        assert_eq!(wheel.steps(30, 0, 3.75), 0);
        for _ in 0..5 {
            assert_eq!(wheel.steps(15, 0, 1.88), 0);
        }
        assert_eq!(wheel.steps(15, 0, 1.88), 1);
        assert_eq!(wheel.steps(-120, 0, -15.0), -1);
    }

    #[test]
    fn legacy_wheel_data_keeps_its_step_count() {
        let mut wheel = WheelStepAccumulator::default();
        assert_eq!(wheel.steps(0, 2, 0.0), 1);
        assert_eq!(wheel.steps(0, -1, 0.0), -1);
        assert_eq!(wheel.steps(0, 0, 8.0), 1);
        assert_eq!(wheel.steps(0, 0, -8.0), -1);
    }
}

fn apply_input_region(
    qh: &QueueHandle<WaylandState>,
    compositor: &CompositorState,
    surface: &SurfaceEntry,
) {
    let Some(layer_surface) = &surface.layer_surface else {
        return;
    };
    if surface.visible && surface.input.pointer && surface.capture_outside {
        layer_surface.wl_surface().set_input_region(None);
        return;
    }
    let region = compositor.wl_compositor().create_region(qh, ());
    if surface.visible
        && surface.input.pointer
        && let Some(rect) = surface.input_rect
    {
        region.add(
            rect.x as i32,
            rect.y as i32,
            rect.width as i32,
            rect.height as i32,
        );
    }
    layer_surface.wl_surface().set_input_region(Some(&region));
    region.destroy();
}

struct WaylandState {
    registry: RegistryState,
    compositor: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm: Shm,
    layer_shell: LayerShell,
    specs: Vec<SurfaceSpec>,
    outputs: HashMap<u32, OutputRecord>,
    surfaces: Vec<SurfaceEntry>,
    next_surface_id: usize,
    pointer: Option<ThemedPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    event_tx: Sender<UiEvent>,
    pointer_x: f64,
    pointer_y: f64,
    pointer_surface: Option<usize>,
    wheel_steps: WheelStepAccumulator,
    /// Where key events go, tracked separately from the pointer.
    ///
    /// The two focuses move independently: the pointer leaves the panel while
    /// the panel still holds the keyboard. Routing keys by pointer instead
    /// silently dropped them, and an exclusive layer surface holding a keyboard
    /// it then ignores swallows Escape along with everything else.
    keyboard_surface: Option<usize>,
    _virtual_pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    virtual_pointer: Option<ZwlrVirtualPointerV1>,
    started_at: Instant,
}

impl WaylandState {
    fn map_popup_surface(&mut self, index: usize, qh: &QueueHandle<Self>) -> Result<()> {
        let Some(entry) = self.surfaces.get(index) else {
            return Ok(());
        };
        if entry.kind != SurfaceKind::Popup || entry.layer_surface.is_some() {
            return Ok(());
        }
        let output_name = entry.output.clone();
        let output = self
            .outputs
            .values()
            .find(|output| output.name == output_name)
            .map(|output| output.output.clone())
            .with_context(|| format!("popup output {output_name} disappeared"))?;

        let surface = self.compositor.create_surface(qh);
        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("niri-xmb-popup"),
            Some(&output),
        );
        layer_surface.set_anchor(Anchor::all());
        layer_surface.set_size(0, 0);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer_surface.commit();

        self.surfaces[index].layer_surface = Some(layer_surface);
        Ok(())
    }

    fn refresh_output(&mut self, output: &wl_output::WlOutput) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };
        let Some(name) = info.name else {
            return;
        };
        if ensure_popup_spec(&mut self.specs, &name) {
            tracing::info!(output = %name, "registered popup surface for connected output");
        }
        let (x, y) = info.logical_position.unwrap_or((0, 0));
        let (width, height) = info.logical_size.unwrap_or((1920, 1080));
        for surface in &mut self.surfaces {
            if surface.output == name {
                surface.output_x = x;
                surface.output_y = y;
                surface.output_width = width;
                surface.output_height = height;
            }
        }
        self.outputs.insert(
            output.id().protocol_id(),
            OutputRecord {
                output: output.clone(),
                name,
                x,
                y,
                width,
                height,
            },
        );
    }

    fn sync_surfaces(&mut self, qh: &QueueHandle<Self>) {
        let specs = self.specs.clone();
        for spec in specs {
            let exists = self.surfaces.iter().any(|surface| {
                surface.output == spec.output
                    && matches!(
                        (&surface.kind, &spec.kind),
                        (SurfaceKind::Popup, SurfaceSpecKind::Popup)
                            | (SurfaceKind::Bar(_), SurfaceSpecKind::Bar { .. })
                    )
                    && match (&surface.kind, &spec.kind) {
                        (SurfaceKind::Bar(existing), SurfaceSpecKind::Bar { index, .. }) => {
                            existing == index
                        }
                        _ => true,
                    }
            });
            if exists {
                continue;
            }
            let Some(output) = self
                .outputs
                .values()
                .find(|output| output.name == spec.output)
            else {
                continue;
            };

            let surface = self.compositor.create_surface(qh);
            let (kind, layer, namespace) = match &spec.kind {
                SurfaceSpecKind::Bar { index, .. } => {
                    (SurfaceKind::Bar(*index), Layer::Overlay, "niri-xmb-bar")
                }
                SurfaceSpecKind::Popup => (SurfaceKind::Popup, Layer::Overlay, "niri-xmb-popup"),
            };
            let layer_surface = self.layer_shell.create_layer_surface(
                qh,
                surface,
                layer,
                Some(namespace),
                Some(&output.output),
            );

            match &spec.kind {
                SurfaceSpecKind::Bar { config, .. } => configure_bar(&layer_surface, config),
                SurfaceSpecKind::Popup => {
                    layer_surface.set_anchor(Anchor::all());
                    layer_surface.set_size(0, 0);
                    layer_surface.set_exclusive_zone(-1);
                    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
                }
            }
            layer_surface.commit();

            let Ok(pool) = SlotPool::new(8 * 1024 * 1024, &self.shm) else {
                tracing::warn!(output = %output.name, "failed to create SHM pool");
                continue;
            };
            let id = self.next_surface_id;
            self.next_surface_id += 1;
            self.surfaces.push(SurfaceEntry {
                id,
                kind,
                output: output.name.clone(),
                output_x: output.x,
                output_y: output.y,
                output_width: output.width,
                output_height: output.height,
                layer_surface: Some(layer_surface),
                pool,
                width: 0,
                height: 0,
                configured: false,
                visible: kind != SurfaceKind::Popup,
                input: PopupInput::NONE,
                capture_outside: false,
                input_rect: None,
                frame_ready: true,
                buffers: Vec::new(),
                last_damage: None,
            });
        }
    }

    fn surface_for_wl(&self, wl_surface: &wl_surface::WlSurface) -> Option<&SurfaceEntry> {
        self.surfaces.iter().find(|surface| {
            surface
                .layer_surface
                .as_ref()
                .is_some_and(|layer_surface| layer_surface.wl_surface() == wl_surface)
        })
    }

    fn emit(&self, surface_id: usize, event: UiEventKind) {
        let Some(surface) = self
            .surfaces
            .iter()
            .find(|surface| surface.id == surface_id)
        else {
            return;
        };
        let origin = surface_origin(surface, &self.specs);
        let _ = self.event_tx.try_send(UiEvent {
            surface_id,
            kind: surface.kind,
            output: surface.output.clone(),
            x: self.pointer_x,
            y: self.pointer_y,
            global_x: surface.output_x as f64 + origin.0 + self.pointer_x,
            global_y: surface.output_y as f64 + origin.1 + self.pointer_y,
            event,
        });
    }
}

fn configure_bar(layer_surface: &SctLayerSurface, config: &BarConfig) {
    let anchor = match (config.edge, config.align) {
        (Edge::Top, Align::Start) => Anchor::TOP | Anchor::LEFT,
        (Edge::Top, Align::Center) => Anchor::TOP,
        (Edge::Top, Align::End) => Anchor::TOP | Anchor::RIGHT,
        (Edge::Bottom, Align::Start) => Anchor::BOTTOM | Anchor::LEFT,
        (Edge::Bottom, Align::Center) => Anchor::BOTTOM,
        (Edge::Bottom, Align::End) => Anchor::BOTTOM | Anchor::RIGHT,
        (Edge::Left, Align::Start) => Anchor::LEFT | Anchor::TOP,
        (Edge::Left, Align::Center) => Anchor::LEFT,
        (Edge::Left, Align::End) => Anchor::LEFT | Anchor::BOTTOM,
        (Edge::Right, Align::Start) => Anchor::RIGHT | Anchor::TOP,
        (Edge::Right, Align::Center) => Anchor::RIGHT,
        (Edge::Right, Align::End) => Anchor::RIGHT | Anchor::BOTTOM,
    };
    layer_surface.set_anchor(anchor);
    layer_surface.set_size(config.width, config.height);
    layer_surface.set_exclusive_zone(0);
    layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
    let (top, right, bottom, left) = match (config.edge, config.align) {
        (Edge::Top, Align::Start) => (config.margin, 0, 0, config.offset),
        (Edge::Top, Align::Center) => (config.margin, 0, 0, 0),
        (Edge::Top, Align::End) => (config.margin, config.offset, 0, 0),
        (Edge::Bottom, Align::Start) => (0, 0, config.margin, config.offset),
        (Edge::Bottom, Align::Center) => (0, 0, config.margin, 0),
        (Edge::Bottom, Align::End) => (0, config.offset, config.margin, 0),
        (Edge::Left, Align::Start) => (config.offset, 0, 0, config.margin),
        (Edge::Left, Align::Center) => (0, 0, 0, config.margin),
        (Edge::Left, Align::End) => (0, 0, config.offset, config.margin),
        (Edge::Right, Align::Start) => (config.offset, config.margin, 0, 0),
        (Edge::Right, Align::Center) => (0, config.margin, 0, 0),
        (Edge::Right, Align::End) => (0, config.margin, config.offset, 0),
    };
    layer_surface.set_margin(top, right, bottom, left);
}

fn surface_origin(surface: &SurfaceEntry, specs: &[SurfaceSpec]) -> (f64, f64) {
    let SurfaceKind::Bar(index) = surface.kind else {
        return (0.0, 0.0);
    };
    let Some(config) = specs.iter().find_map(|spec| match &spec.kind {
        SurfaceSpecKind::Bar {
            index: spec_index,
            config,
        } if *spec_index == index => Some(config),
        _ => None,
    }) else {
        return (0.0, 0.0);
    };
    let x = match (config.edge, config.align) {
        (Edge::Top | Edge::Bottom, Align::Start) => config.offset as f64,
        (Edge::Top | Edge::Bottom, Align::Center) => {
            (surface.output_width - surface.width as i32) as f64 * 0.5
        }
        (Edge::Top | Edge::Bottom, Align::End) => {
            (surface.output_width - surface.width as i32 - config.offset) as f64
        }
        (Edge::Left, _) => config.margin as f64,
        (Edge::Right, _) => (surface.output_width - surface.width as i32 - config.margin) as f64,
    };
    let y = match (config.edge, config.align) {
        (Edge::Left | Edge::Right, Align::Start) => config.offset as f64,
        (Edge::Left | Edge::Right, Align::Center) => {
            (surface.output_height - surface.height as i32) as f64 * 0.5
        }
        (Edge::Left | Edge::Right, Align::End) => {
            (surface.output_height - surface.height as i32 - config.offset) as f64
        }
        (Edge::Top, _) => config.margin as f64,
        (Edge::Bottom, _) => (surface.output_height - surface.height as i32 - config.margin) as f64,
    };
    (x, y)
}

impl CompositorHandler for WaylandState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        wl_surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if let Some(surface) = self.surfaces.iter_mut().find(|surface| {
            surface
                .layer_surface
                .as_ref()
                .is_some_and(|layer_surface| layer_surface.wl_surface() == wl_surface)
        }) {
            surface.frame_ready = true;
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for WaylandState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.refresh_output(&output);
        self.sync_surfaces(qh);
    }

    fn update_output(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.refresh_output(&output);
        self.sync_surfaces(qh);
    }

    fn output_destroyed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        let id = output.id().protocol_id();
        if let Some(record) = self.outputs.remove(&id) {
            self.surfaces
                .retain(|surface| surface.output != record.name);
        }
    }
}

impl SeatHandler for WaylandState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            let cursor_surface = self.compositor.create_surface(qh);
            self.pointer = self
                .seat_state
                .get_pointer_with_theme(
                    qh,
                    &seat,
                    self.shm.wl_shm(),
                    cursor_surface,
                    ThemeSpec::System,
                )
                .ok();
        }
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.pointer = None;
        }
        if capability == Capability::Keyboard {
            self.keyboard = None;
        }
    }
}

impl PointerHandler for WaylandState {
    fn pointer_frame(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let surface_id = self
                .surface_for_wl(&event.surface)
                .map(|surface| surface.id);
            match event.kind {
                PointerEventKind::Enter { .. } => {
                    if let Some(pointer) = &self.pointer {
                        let _ = pointer.set_cursor(connection, CursorIcon::Default);
                    }
                    self.pointer_x = event.position.0;
                    self.pointer_y = event.position.1;
                    self.pointer_surface = surface_id;
                    if let Some(id) = surface_id {
                        self.emit(id, UiEventKind::Enter);
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if let Some(id) = self.pointer_surface.take().or(surface_id) {
                        self.emit(id, UiEventKind::Leave);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    self.pointer_x = event.position.0;
                    self.pointer_y = event.position.1;
                    if let Some(id) = self.pointer_surface.or(surface_id) {
                        self.emit(id, UiEventKind::Motion);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if let Some(id) = self.pointer_surface.or(surface_id) {
                        self.emit(id, UiEventKind::Press { button });
                    }
                }
                PointerEventKind::Release { button, .. } => {
                    if let Some(id) = self.pointer_surface.or(surface_id) {
                        self.emit(id, UiEventKind::Release { button });
                    }
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let steps = self.wheel_steps.steps(
                        vertical.value120,
                        vertical.discrete,
                        vertical.absolute,
                    );
                    if steps != 0
                        && let Some(id) = self.pointer_surface.or(surface_id)
                    {
                        self.emit(id, UiEventKind::Scroll { steps });
                    }
                    let _ = horizontal;
                }
            }
        }
    }
}

impl KeyboardHandler for WaylandState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.keyboard_surface = self.surface_for_wl(surface).map(|surface| surface.id);
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.surface_for_wl(surface).map(|surface| surface.id) == self.keyboard_surface {
            self.keyboard_surface = None;
        }
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(id) = self.keyboard_surface {
            self.emit(
                id,
                UiEventKind::Key {
                    keysym: event.keysym.raw(),
                },
            );
        }
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _: u32,
    ) {
    }
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(id) = self.keyboard_surface {
            self.emit(
                id,
                UiEventKind::Key {
                    keysym: event.keysym.raw(),
                },
            );
        }
    }
    fn update_repeat_info(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: RepeatInfo,
    ) {
    }
}

impl LayerShellHandler for WaylandState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer_surface: &SctLayerSurface) {
        self.surfaces.retain(|surface| {
            !surface
                .layer_surface
                .as_ref()
                .is_some_and(|candidate| candidate == layer_surface)
        });
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer_surface: &SctLayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(surface) = self.surfaces.iter_mut().find(|surface| {
            surface
                .layer_surface
                .as_ref()
                .is_some_and(|candidate| candidate == layer_surface)
        }) {
            let old_width = surface.width;
            let old_height = surface.height;
            let width = if configure.new_size.0 > 0 {
                configure.new_size.0
            } else {
                old_width
            };
            let height = if configure.new_size.1 > 0 {
                configure.new_size.1
            } else {
                old_height
            };
            if width != old_width || height != old_height {
                tracing::debug!(
                    output = %surface.output,
                    old_width,
                    old_height,
                    width,
                    height,
                    "layer surface size changed"
                );
                surface.last_damage = None;
                surface.input_rect = None;
            }
            surface.width = width;
            surface.height = height;
            surface.configured = true;
        }
    }
}

impl ShmHandler for WaylandState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl wayland_client::Dispatch<wl_region::WlRegion, ()> for WaylandState {
    fn event(
        _: &mut Self,
        _: &wl_region::WlRegion,
        _: wl_region::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl ProvidesRegistryState for WaylandState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(WaylandState);
smithay_client_toolkit::delegate_output!(WaylandState);
smithay_client_toolkit::delegate_seat!(WaylandState);
smithay_client_toolkit::delegate_keyboard!(WaylandState);
smithay_client_toolkit::delegate_pointer!(WaylandState);
smithay_client_toolkit::delegate_layer!(WaylandState);
smithay_client_toolkit::delegate_shm!(WaylandState);
smithay_client_toolkit::delegate_registry!(WaylandState);
wayland_client::delegate_noop!(WaylandState: ignore ZwlrVirtualPointerManagerV1);
wayland_client::delegate_noop!(WaylandState: ignore ZwlrVirtualPointerV1);
