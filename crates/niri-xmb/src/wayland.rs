//! Multi-output wlr-layer-shell surfaces adapted from Woven's standalone bar and overlay.

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
    shm::{Shm, ShmHandler, slot::SlotPool},
};
use std::collections::HashMap;
use std::time::Instant;
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
}

#[derive(Debug, Clone)]
pub enum UiEventKind {
    Enter,
    Leave,
    Motion,
    Press { button: u32 },
    Release { button: u32 },
    Scroll { dy: f64 },
    Key { keysym: u32 },
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

struct OutputRecord {
    output: wl_output::WlOutput,
    name: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

struct SurfaceEntry {
    id: usize,
    kind: SurfaceKind,
    output: String,
    output_x: i32,
    output_y: i32,
    output_width: i32,
    output_height: i32,
    layer_surface: SctLayerSurface,
    pool: SlotPool,
    width: u32,
    height: u32,
    configured: bool,
    visible: bool,
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
            specs.push(SurfaceSpec {
                output: output.clone(),
                kind: SurfaceSpecKind::Popup,
            });
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
            _virtual_pointer_manager: virtual_pointer_manager,
            virtual_pointer,
            started_at: Instant::now(),
        };

        let mut manager = Self { queue, state };
        manager.queue.roundtrip(&mut manager.state)?;
        manager.queue.roundtrip(&mut manager.state)?;
        manager.state.sync_surfaces(&manager.queue.handle());
        manager.queue.roundtrip(&mut manager.state)?;
        manager.hide_all_popups()?;
        Ok(manager)
    }

    pub fn dispatch(&mut self) -> Result<()> {
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
            let mut fd = PollFd::new(&borrowed, PollFlags::IN);
            let timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            if poll(std::slice::from_mut(&mut fd), Some(&timeout)).unwrap_or(0) > 0 {
                let _ = guard.read();
            } else {
                drop(guard);
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
                }
            })
            .collect()
    }

    pub fn present(&mut self, surface_id: usize, pixels: &[u8]) -> Result<()> {
        let Some(surface) = self
            .state
            .surfaces
            .iter_mut()
            .find(|surface| surface.id == surface_id)
        else {
            return Ok(());
        };
        if !surface.configured || surface.width == 0 || surface.height == 0 {
            return Ok(());
        }
        let stride = surface.width * 4;
        let (buffer, canvas) = surface
            .pool
            .create_buffer(
                surface.width as i32,
                surface.height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .context("failed to create SHM buffer")?;
        copy_rgba_to_argb8888(canvas, pixels);
        let wl_surface = surface.layer_surface.wl_surface();
        buffer
            .attach_to(wl_surface)
            .context("failed to attach SHM buffer")?;
        wl_surface.damage_buffer(0, 0, surface.width as i32, surface.height as i32);
        surface.layer_surface.commit();
        Ok(())
    }

    pub fn show_popups(&mut self, outputs: &[String]) -> Result<()> {
        self.show_popups_with(outputs, true)
    }

    /// Show the popup on `outputs`.
    ///
    /// With `interactive` false the surface takes neither pointer nor keyboard
    /// input: that is what makes the peek overlay a passive readout, letting
    /// the compositor keybinds the user is pressing continue to reach Niri and
    /// clicks pass through to whatever is underneath.
    pub fn show_popups_with(&mut self, outputs: &[String], interactive: bool) -> Result<()> {
        for surface in &mut self.state.surfaces {
            if surface.kind != SurfaceKind::Popup {
                continue;
            }
            let show = outputs.iter().any(|output| output == &surface.output);
            surface.visible = show;
            set_input_region(
                &self.queue.handle(),
                &self.state.compositor,
                surface,
                show && interactive,
            );
            surface
                .layer_surface
                .set_keyboard_interactivity(if show && interactive {
                    KeyboardInteractivity::Exclusive
                } else {
                    KeyboardInteractivity::None
                });
            surface.layer_surface.commit();
        }
        self.queue.roundtrip(&mut self.state)?;
        Ok(())
    }

    pub fn hide_all_popups(&mut self) -> Result<()> {
        self.show_popups(&[])
    }

    pub fn cursor_restore_available(&self) -> bool {
        self.state.virtual_pointer.is_some()
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

/// tiny-skia exposes premultiplied RGBA bytes. Wayland's ARGB8888 format is
/// stored as BGRA bytes on little-endian systems, so red and blue must swap at
/// the SHM boundary.
fn copy_rgba_to_argb8888(destination: &mut [u8], source: &[u8]) {
    for (dst, src) in destination.chunks_exact_mut(4).zip(source.chunks_exact(4)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
        dst[3] = src[3];
    }
}

#[cfg(test)]
mod tests {
    use super::copy_rgba_to_argb8888;

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
}

fn set_input_region(
    qh: &QueueHandle<WaylandState>,
    compositor: &CompositorState,
    surface: &SurfaceEntry,
    enabled: bool,
) {
    if enabled {
        surface.layer_surface.wl_surface().set_input_region(None);
        return;
    }
    let region = compositor.wl_compositor().create_region(qh, ());
    surface
        .layer_surface
        .wl_surface()
        .set_input_region(Some(&region));
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
    _virtual_pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    virtual_pointer: Option<ZwlrVirtualPointerV1>,
    started_at: Instant,
}

impl WaylandState {
    fn refresh_output(&mut self, output: &wl_output::WlOutput) {
        let Some(info) = self.output_state.info(output) else {
            return;
        };
        let Some(name) = info.name else {
            return;
        };
        let (x, y) = info.logical_position.unwrap_or((0, 0));
        let (width, height) = info.logical_size.unwrap_or((1920, 1080));
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
                layer_surface,
                pool,
                width: 0,
                height: 0,
                configured: false,
                visible: kind != SurfaceKind::Popup,
            });
        }
    }

    fn surface_for_wl(&self, wl_surface: &wl_surface::WlSurface) -> Option<&SurfaceEntry> {
        self.surfaces
            .iter()
            .find(|surface| surface.layer_surface.wl_surface() == wl_surface)
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.refresh_output(&output);
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
                    if let Some(id) = self.pointer_surface.or(surface_id) {
                        self.emit(
                            id,
                            UiEventKind::Scroll {
                                dy: vertical.absolute,
                            },
                        );
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
        self.pointer_surface = self.surface_for_wl(surface).map(|surface| surface.id);
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if let Some(id) = self.pointer_surface {
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
        if let Some(id) = self.pointer_surface {
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
        self.surfaces
            .retain(|surface| surface.layer_surface.wl_surface() != layer_surface.wl_surface());
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer_surface: &SctLayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(surface) = self
            .surfaces
            .iter_mut()
            .find(|surface| surface.layer_surface.wl_surface() == layer_surface.wl_surface())
        {
            if configure.new_size.0 > 0 {
                surface.width = configure.new_size.0;
            }
            if configure.new_size.1 > 0 {
                surface.height = configure.new_size.1;
            }
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
