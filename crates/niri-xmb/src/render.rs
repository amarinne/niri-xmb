use crate::anim::{Animator, Timing, lerp};
use crate::config::{
    AnchorX, AnchorY, BarConfig, Config, Edge, PopupOutputs, PopupPosition, UrgentIndicator,
};
use crate::draw::{
    blit_icon_alpha, fill_circle, fill_gradient_rounded_rect, fill_radial_glow, fill_rect,
    fill_rounded_rect, stroke_rounded_rect,
};
use crate::icons::IconCache;
use crate::model::{OutputModel, Snapshot, WindowModel, WindowSlot, WorkspaceModel};
use crate::text::TextRenderer;
use crate::wayland::{DamageRect, SurfaceInfo, SurfaceKind};
use std::collections::HashMap;
use tiny_skia::{FilterQuality, Pixmap, PixmapPaint, Transform};

/// Icon sizing derived from the style config, shared by every popup layout.
#[derive(Debug, Clone, Copy)]
struct IconMetrics {
    /// Size of an unselected icon.
    base: f32,
    /// Size of the selected icon; emphasis by scale replaces the selection box.
    focused: f32,
    /// Horizontal advance between windows in a workspace.
    column_step: f32,
    /// Vertical advance between windows joined into one Niri column.
    stack_step: f32,
    /// Empty space kept between workspace bands.
    workspace_gap: f32,
}

impl IconMetrics {
    fn new(config: &Config) -> Self {
        let base = config.style.icon_size.max(16) as f32;
        let focused = (config.style.icon_size_focused as f32).max(base);
        Self {
            base,
            focused,
            column_step: focused + config.style.column_gap,
            stack_step: base + config.style.stack_gap,
            workspace_gap: config.style.row_gap,
        }
    }

    fn workspace_step(&self, output: &OutputModel) -> f32 {
        let max_stack = output
            .workspaces
            .iter()
            .map(WorkspaceModel::max_stack)
            .max()
            .unwrap_or(1) as f32;
        self.focused + self.stack_step * (max_stack - 1.0) + self.workspace_gap
    }
}

/// Fade an icon out as it approaches the canvas edge.
///
/// The canvas is smaller than its content, so something has to give at the
/// boundary; fading reads as "more content this way", a hard clip reads as a
/// rendering bug.
fn edge_fade(rect: Rect, canvas: Rect, margin: f32) -> f32 {
    let margin = margin.max(1.0);
    let left = (rect.x + rect.width - canvas.x) / margin;
    let right = (canvas.x + canvas.width - rect.x) / margin;
    let top = (rect.y + rect.height - canvas.y) / margin;
    let bottom = (canvas.y + canvas.height - rect.y) / margin;
    left.min(right).min(top).min(bottom).clamp(0.0, 1.0)
}

/// Trim a rect to the canvas bounds, returning a zero-size rect when outside.
fn clamp_rect(rect: Rect, canvas: Rect) -> Rect {
    let x0 = rect.x.max(canvas.x);
    let y0 = rect.y.max(canvas.y);
    let x1 = (rect.x + rect.width).min(canvas.x + canvas.width);
    let y1 = (rect.y + rect.height).min(canvas.y + canvas.height);
    Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(0.0),
        height: (y1 - y0).max(0.0),
    }
}

fn visible_hit_rect(rect: Rect, canvas: Rect) -> Option<Rect> {
    let visible = clamp_rect(rect, canvas);
    (visible.width > 0.0 && visible.height > 0.0).then_some(visible)
}

/// Panel background: soft drop shadow, vertical gradient body, hairline edge.
fn draw_panel_chrome(
    pixmap: &mut Pixmap,
    panel: Rect,
    config: &Config,
    panel_input_active: bool,
    scale: f32,
) {
    let radius = config.style.panel_radius;
    let offset = config.style.shadow_offset;
    let spread = config.style.shadow_spread;
    let shadow = config.style.shadow.get();
    // Two offset passes approximate a blur without a real blur pass. The outer
    // one is spread wider and drawn fainter.
    let mut outer = shadow;
    outer.set_alpha(outer.alpha() * 0.6);
    fill_rounded_rect(
        pixmap,
        panel.x - spread * 0.5,
        panel.y + offset - spread * 0.5,
        panel.width + spread,
        panel.height + spread,
        radius + spread,
        outer,
        scale,
    );
    fill_rounded_rect(
        pixmap,
        panel.x,
        panel.y + offset,
        panel.width,
        panel.height,
        radius,
        shadow,
        scale,
    );
    fill_gradient_rounded_rect(
        pixmap,
        panel.x,
        panel.y,
        panel.width,
        panel.height,
        radius,
        config.style.background.get(),
        config.style.background_end.get(),
        scale,
    );
    stroke_rounded_rect(
        pixmap,
        panel.x + 0.5,
        panel.y + 0.5,
        panel.width - 1.0,
        panel.height - 1.0,
        radius,
        if panel_input_active {
            config.style.accent.get()
        } else {
            config.style.border.get()
        },
        config.style.border_width,
        scale,
    );
}

#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.width) as f64
            && y < (self.y + self.height) as f64
    }
}

fn damage_for_rect(rect: Rect, padding: f32, width: u32, height: u32) -> Option<DamageRect> {
    let x0 = (rect.x - padding).floor().max(0.0) as u32;
    let y0 = (rect.y - padding).floor().max(0.0) as u32;
    let x1 = (rect.x + rect.width + padding)
        .ceil()
        .clamp(0.0, width as f32) as u32;
    let y1 = (rect.y + rect.height + padding)
        .ceil()
        .clamp(0.0, height as f32) as u32;
    (x1 > x0 && y1 > y0).then_some(DamageRect {
        x: x0,
        y: y0,
        width: x1 - x0,
        height: y1 - y0,
    })
}

fn clear_damage(pixmap: &mut Pixmap, damage: DamageRect) {
    let width = pixmap.width() as usize;
    let x0 = damage.x.min(pixmap.width()) as usize * 4;
    let x1 = damage.x.saturating_add(damage.width).min(pixmap.width()) as usize * 4;
    let y0 = damage.y.min(pixmap.height()) as usize;
    let y1 = damage.y.saturating_add(damage.height).min(pixmap.height()) as usize;
    let stride = width * 4;
    for y in y0..y1 {
        pixmap.data_mut()[y * stride + x0..y * stride + x1].fill(0);
    }
}
fn copy_damage(source: &Pixmap, destination: &mut Pixmap, damage: DamageRect) {
    let source_stride = source.width() as usize * 4;
    let destination_stride = destination.width() as usize * 4;
    let x = damage.x as usize * 4;
    let width = damage.width as usize * 4;
    for row in 0..damage.height as usize {
        let source_start = (damage.y as usize + row) * source_stride + x;
        let destination_start = row * destination_stride;
        destination.data_mut()[destination_start..destination_start + width]
            .copy_from_slice(&source.data()[source_start..source_start + width]);
    }
}

pub struct RenderedFrame<'a> {
    pub pixels: &'a [u8],
    pub damage: Option<DamageRect>,
}

#[derive(Debug, Clone)]
pub enum HitTarget {
    Panel,
    Workspace {
        workspace_id: u64,
    },
    Canvas {
        workspace_id: u64,
        rect: Rect,
    },
    Window {
        workspace_id: u64,
        window_id: u64,
        rect: Rect,
        canvas: Option<Rect>,
    },
}

#[derive(Debug, Clone)]
pub struct HitZone {
    pub rect: Rect,
    pub target: HitTarget,
}

#[derive(Debug, Clone)]
pub struct DragVisual {
    pub window_id: u64,
    pub app_id: String,
    pub global_x: f64,
    pub global_y: f64,
}

#[derive(Debug, Clone)]
pub struct SelectionVisual {
    pub output: String,
    pub workspace_id: u64,
    pub window_id: Option<u64>,
    /// Staged window id per workspace, for workspaces the popup session has
    /// already visited. Ids survive a window reorder. Absent or missing ids
    /// fall back to the workspace's own active window.
    pub anchors: HashMap<u64, u64>,
}

impl SelectionVisual {
    /// Index that should sit on the focal column for `workspace`.
    pub fn anchor(&self, workspace: &WorkspaceModel) -> usize {
        match self.anchors.get(&workspace.id) {
            Some(staged) => workspace
                .windows()
                .position(|window| window.id == *staged)
                .unwrap_or_else(|| workspace.active_index()),
            _ => workspace.active_index(),
        }
    }

    pub fn anchor_slot(&self, workspace: &WorkspaceModel) -> WindowSlot {
        workspace
            .window_slot_at(self.anchor(workspace))
            .unwrap_or_else(|| workspace.active_slot())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HoverTarget {
    Workspace(u64),
    Window(u64),
}

pub struct Renderer {
    text: TextRenderer,
    icons: IconCache,
    hit_zones: HashMap<usize, Vec<HitZone>>,
    popup_origins: HashMap<String, (f32, f32)>,
    hovered_surface: Option<usize>,
    hovered_target: Option<HoverTarget>,
    render_surface: Option<usize>,
    /// Device pixels per logical pixel for the surface being rendered.
    /// Layout stays in logical units; this is what maps it onto the buffer.
    render_scale: f32,
    panel_input_active: bool,
    anim: Animator,
    /// Scratch buffers and damage history belong to one Wayland surface.
    ///
    /// Bars and popups normally have different sizes. Sharing one canvas made
    /// the render loop replace that canvas each time it crossed a surface-size
    /// boundary, which was especially expensive during popup animation.
    surfaces: HashMap<usize, SurfaceRenderState>,
    /// Surfaces already presented blank, so they can be skipped.
    blanked: std::collections::HashSet<usize>,
}

#[derive(Default)]
struct SurfaceRenderState {
    canvas: Option<Pixmap>,
    layer: Option<Pixmap>,
    canvas_damage: Option<DamageRect>,
    bar_key: Option<BarRenderKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BarRenderKey {
    generation: u64,
    width: u32,
    height: u32,
    scale: u32,
    hover: Option<HoverTarget>,
    drag: Option<DragRenderKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DragRenderKey {
    window_id: u64,
    x_bits: u64,
    y_bits: u64,
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            text: TextRenderer::new(),
            icons: IconCache::new(),
            hit_zones: HashMap::new(),
            popup_origins: HashMap::new(),
            hovered_surface: None,
            hovered_target: None,
            render_surface: None,
            render_scale: 1.0,
            panel_input_active: false,
            anim: Animator::new(),
            surfaces: HashMap::new(),
            blanked: std::collections::HashSet::new(),
        }
    }

    /// Set whether the popup is in an input-owning mode. Returns true when a
    /// redraw is needed to update the panel border.
    pub fn set_panel_input_active(&mut self, active: bool) -> bool {
        if self.panel_input_active == active {
            return false;
        }
        self.panel_input_active = active;
        true
    }

    /// Recompute every animation target from current state and advance the
    /// clock. Returns true while anything is still moving, which the caller
    /// uses to keep redrawing.
    ///
    /// Targets are derived here rather than during drawing so that rendering
    /// stays a pure function of animation state.
    pub fn update_animations(
        &mut self,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        open_outputs: &std::collections::HashSet<String>,
    ) -> bool {
        self.anim.begin_sync();
        for output in snapshot.outputs.keys() {
            self.anim
                .set_panel_open(output, open_outputs.contains(output));
        }
        // A popup surface can outlive its output in the snapshot; keep any
        // panel that is still open so it does not vanish mid-transition.
        for output in open_outputs {
            self.anim.set_panel_open(output, true);
        }

        let metrics = IconMetrics::new(config);
        for output in snapshot.outputs.values() {
            let workspace_step = metrics.workspace_step(output);
            let selected = selection.filter(|selection| selection.output == output.name);
            // A canvas the selection is not on still centres something useful:
            // its own active workspace.
            let mut focal_row = output
                .workspaces
                .iter()
                .position(|workspace| workspace.active || workspace.focused)
                .unwrap_or(0);
            for (index, workspace) in output.workspaces.iter().enumerate() {
                let is_selected =
                    selected.is_some_and(|selection| selection.workspace_id == workspace.id);
                let emphasized = is_selected
                    || self.workspace_hovered(workspace.id)
                    || workspace.active
                    || workspace.focused;
                self.anim.set_workspace_emphasis(workspace.id, emphasized);
                if is_selected {
                    focal_row = index;
                }

                // Which window this row parks on the focal column. For the
                // selected row that is the selection; for every other row its
                // own staged-or-active window. Each row keeps its own track, so
                // becoming selected does not change where the row already sits.
                let mut focal_slot = selected
                    .map(|selection| selection.anchor_slot(workspace))
                    .unwrap_or_else(|| workspace.active_slot());
                for window in workspace.windows() {
                    let focused = selected
                        .and_then(|selection| selection.window_id)
                        .is_some_and(|id| id == window.id);
                    self.anim
                        .set_window_emphasis(window.id, focused || self.window_hovered(window.id));
                    if is_selected && focused {
                        focal_slot = workspace.window_slot(window.id).unwrap_or(focal_slot);
                    }
                }
                self.anim
                    .set_row_scroll(workspace.id, focal_slot.column as f32 * metrics.column_step);
                self.anim
                    .set_stack_scroll(workspace.id, focal_slot.row as f32 * metrics.stack_step);
            }
            self.anim
                .set_column_scroll(&output.name, focal_row as f32 * workspace_step);
        }
        self.anim.retain_scrolls(
            |output| snapshot.outputs.contains_key(output),
            |workspace| snapshot.workspace(workspace).is_some(),
        );
        self.anim.tick(Timing::from_config(&config.animation))
    }

    pub fn set_popup_cursor_origin(&mut self, output: &str, x: f64, y: f64) {
        self.popup_origins
            .insert(output.to_string(), (x as f32, y as f32));
    }

    pub fn hit_test(&self, surface_id: usize, x: f64, y: f64) -> Option<&HitTarget> {
        self.hit_zones
            .get(&surface_id)?
            .iter()
            .rev()
            .find(|zone| zone.rect.contains(x, y))
            .map(|zone| &zone.target)
    }

    /// Workspace under the pointer on a bar or popup canvas.
    ///
    /// A bar window icon belongs to its workspace too. Wheel navigation uses
    /// this id as its gesture start, so scrolling an icon and scrolling the
    /// empty part of the same workspace cell have the same target.
    pub fn workspace_at(&self, surface_id: usize, x: f64, y: f64) -> Option<u64> {
        match self.hit_test(surface_id, x, y)? {
            HitTarget::Workspace { workspace_id, .. }
            | HitTarget::Canvas { workspace_id, .. }
            | HitTarget::Window { workspace_id, .. } => Some(*workspace_id),
            HitTarget::Panel => None,
        }
    }
    pub fn panel_input_rect(
        &self,
        surface_id: usize,
        width: u32,
        height: u32,
    ) -> Option<DamageRect> {
        let panel = self
            .hit_zones
            .get(&surface_id)?
            .iter()
            .find_map(|zone| matches!(zone.target, HitTarget::Panel).then_some(zone.rect))?;
        damage_for_rect(panel, 0.0, width, height)
    }

    pub fn update_hover(&mut self, surface_id: usize, x: f64, y: f64) -> bool {
        let target = self
            .hit_test(surface_id, x, y)
            .and_then(|target| match target {
                HitTarget::Workspace { workspace_id, .. }
                | HitTarget::Canvas { workspace_id, .. } => {
                    Some(HoverTarget::Workspace(*workspace_id))
                }
                HitTarget::Window { window_id, .. } => Some(HoverTarget::Window(*window_id)),
                HitTarget::Panel => None,
            });
        let changed = self.hovered_surface != Some(surface_id) || self.hovered_target != target;
        self.hovered_surface = Some(surface_id);
        self.hovered_target = target;
        changed
    }

    pub fn clear_hover(&mut self, surface_id: usize) -> bool {
        if self.hovered_surface != Some(surface_id) {
            return false;
        }
        let changed = self.hovered_target.is_some();
        self.hovered_surface = None;
        self.hovered_target = None;
        changed
    }

    fn workspace_hovered(&self, workspace_id: u64) -> bool {
        self.hovered_surface == self.render_surface
            && self.hovered_target == Some(HoverTarget::Workspace(workspace_id))
    }

    fn window_hovered(&self, window_id: u64) -> bool {
        self.hovered_surface == self.render_surface
            && self.hovered_target == Some(HoverTarget::Window(window_id))
    }

    pub fn hit_test_global<'a>(
        &'a self,
        surfaces: &'a [SurfaceInfo],
        global_x: f64,
        global_y: f64,
    ) -> Option<(&'a SurfaceInfo, &'a HitTarget, f64, f64)> {
        for surface in surfaces.iter().rev() {
            if !surface.configured {
                continue;
            }
            let local_x = global_x - surface.global_x;
            let local_y = global_y - surface.global_y;
            if local_x < 0.0
                || local_y < 0.0
                || local_x >= surface.width as f64
                || local_y >= surface.height as f64
            {
                continue;
            }
            if let Some(target) = self.hit_test(surface.id, local_x, local_y) {
                return Some((surface, target, local_x, local_y));
            }
        }
        None
    }

    /// Drop render state for surfaces that are no longer visible.
    ///
    /// Popup canvases are output-sized. A follow-active popup can visit many
    /// outputs during one daemon lifetime, so retaining every old canvas would
    /// turn the per-surface CPU fix into steady memory growth.
    pub fn retain_surfaces(&mut self, surfaces: &[SurfaceInfo]) {
        let live = surfaces
            .iter()
            .map(|surface| surface.id)
            .collect::<std::collections::HashSet<_>>();
        self.surfaces.retain(|id, _| live.contains(id));
        self.hit_zones.retain(|id, _| live.contains(id));
        self.blanked.retain(|id| live.contains(id));
    }

    /// Render `surface`, returning its pixels, or `None` when there is nothing
    /// new to present.
    ///
    /// A popup layer surface spans its whole output, so a naive implementation
    /// allocates, clears, and uploads a full-screen buffer every frame for
    /// every monitor — including monitors showing no popup at all. Both are
    /// avoided here: the scratch pixmap is reused across frames, and a surface
    /// that is blank and was already blank last frame is skipped entirely.
    pub fn render(
        &mut self,
        surface: &SurfaceInfo,
        snapshot: &Snapshot,
        config: &Config,
        drag: Option<&DragVisual>,
        selection: Option<&SelectionVisual>,
    ) -> Option<RenderedFrame<'_>> {
        let (width, height) = (surface.width.max(1), surface.height.max(1));
        // The compositor is told this scale via set_buffer_scale, so the
        // buffer must be this many device pixels per logical pixel.
        let scale = surface.scale_factor.max(1) as u32;
        self.render_scale = scale as f32;
        let (device_width, device_height) = (width * scale, height * scale);
        let bar_key = matches!(surface.kind, SurfaceKind::Bar(_)).then(|| BarRenderKey {
            generation: snapshot.generation,
            width,
            height,
            scale,
            hover: (self.hovered_surface == Some(surface.id))
                .then_some(self.hovered_target)
                .flatten(),
            drag: drag.map(|drag| DragRenderKey {
                window_id: drag.window_id,
                x_bits: drag.global_x.to_bits(),
                y_bits: drag.global_y.to_bits(),
            }),
        });
        if let Some(key) = bar_key
            && !surface.needs_full_redraw
            && self
                .surfaces
                .get(&surface.id)
                .is_some_and(|state| state.bar_key == Some(key))
        {
            // Popup animation marks the controller dirty every frame. A bar
            // whose model, hover, size, and drag state did not change has no
            // reason to join that work.
            return None;
        }
        let progress = match surface.kind {
            SurfaceKind::Bar(_) => 1.0,
            SurfaceKind::Popup => self.anim.panel(&surface.output),
        };
        let blank = progress <= 0.004 && drag.is_none();
        if blank && !surface.needs_full_redraw && !self.blanked.insert(surface.id) {
            // Already cleared on a previous frame; nothing to upload.
            return None;
        }
        if !blank {
            self.blanked.remove(&surface.id);
        }

        let mut state = self.surfaces.remove(&surface.id).unwrap_or_default();
        let reusable = state.canvas.as_ref().is_some_and(|pixmap| {
            pixmap.width() == device_width && pixmap.height() == device_height
        });
        if !reusable {
            state.canvas = Pixmap::new(device_width, device_height);
            state.canvas_damage = None;
        }
        let Some(mut pixmap) = state.canvas.take() else {
            self.surfaces.insert(surface.id, state);
            return None;
        };
        if let Some(previous) = state.canvas_damage.take() {
            clear_damage(&mut pixmap, previous);
        }
        self.render_surface = Some(surface.id);
        let mut zones = Vec::new();
        let mut damage = None;

        match surface.kind {
            SurfaceKind::Bar(index) => {
                if let (Some(bar), Some(output)) = (
                    config.bars.get(index),
                    snapshot.outputs.get(&surface.output),
                ) {
                    self.render_bar(&mut pixmap, output, bar, config, &mut zones);
                    damage = Some(DamageRect {
                        x: 0,
                        y: 0,
                        width: device_width,
                        height: device_height,
                    });
                }
            }
            SurfaceKind::Popup => {
                if progress > 0.004 {
                    let panel = self.render_popup_layer(
                        &mut pixmap,
                        &mut state.layer,
                        &surface.output,
                        snapshot,
                        config,
                        selection,
                        progress,
                        &mut zones,
                    );
                    damage = panel.and_then(|panel| {
                        let padding =
                            config.style.shadow_spread + config.style.shadow_offset.abs() + 4.0;
                        let device_panel = Rect {
                            x: panel.x * self.render_scale,
                            y: panel.y * self.render_scale,
                            width: panel.width * self.render_scale,
                            height: panel.height * self.render_scale,
                        };
                        damage_for_rect(
                            device_panel,
                            padding * self.render_scale,
                            device_width,
                            device_height,
                        )
                    });
                }
            }
        }

        if let Some(drag) = drag
            && let Some(rect) = self.render_drag_ghost(&mut pixmap, surface, drag, config)
            && let Some(ghost) = {
                let device_rect = Rect {
                    x: rect.x * self.render_scale,
                    y: rect.y * self.render_scale,
                    width: rect.width * self.render_scale,
                    height: rect.height * self.render_scale,
                };
                damage_for_rect(
                    device_rect,
                    6.0 * self.render_scale,
                    device_width,
                    device_height,
                )
            }
        {
            damage = Some(damage.map_or(ghost, |current| current.union(ghost)));
        }
        self.hit_zones.insert(surface.id, zones);
        state.canvas_damage = damage;
        state.canvas = Some(pixmap);
        state.bar_key = bar_key;
        self.surfaces.insert(surface.id, state);
        self.surfaces
            .get(&surface.id)?
            .canvas
            .as_ref()
            .map(|pixmap| RenderedFrame {
                pixels: pixmap.data(),
                damage,
            })
    }

    fn render_bar(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        bar: &BarConfig,
        config: &Config,
        zones: &mut Vec<HitZone>,
    ) {
        let horizontal = matches!(bar.edge, Edge::Top | Edge::Bottom);
        let logical_width = pixmap.width() as f32 / self.render_scale;
        let logical_height = pixmap.height() as f32 / self.render_scale;
        let length = if horizontal {
            logical_width
        } else {
            logical_height
        };
        let cross = if horizontal {
            logical_height
        } else {
            logical_width
        };
        let gap = config.style.gap;
        let icon_size = config
            .style
            .icon_size
            .min(22)
            .min((cross - 10.0).max(12.0) as u32);
        let label_font = 10.0;
        let workspaces = output
            .workspaces
            .iter()
            .filter(|workspace| {
                !bar.hide_empty_workspaces
                    || workspace.active
                    || workspace.focused
                    || workspace.urgent
                    || workspace.windows().next().is_some()
            })
            .collect::<Vec<_>>();
        let cell_lengths = workspaces
            .iter()
            .map(|workspace| {
                let label = workspace.index.to_string();
                let label_width = if bar.show_workspace_labels {
                    self.text.measure(&label, label_font) + 7.0
                } else {
                    0.0
                };
                let icon_count = workspace.windows().count();
                let icons_width = if icon_count == 0 {
                    0.0
                } else {
                    icon_count as f32 * icon_size as f32 + icon_count.saturating_sub(1) as f32 * 4.0
                };
                (12.0 + label_width + icons_width).max(if bar.show_workspace_labels {
                    30.0
                } else {
                    26.0
                })
            })
            .collect::<Vec<_>>();
        let content_length =
            cell_lengths.iter().sum::<f32>() + gap * cell_lengths.len().saturating_sub(1) as f32;
        let mut main = ((length - content_length) * 0.5).max(0.0);

        for (index, workspace) in workspaces.into_iter().enumerate() {
            let cell_length = cell_lengths[index].min((length - main).max(24.0));
            let cell_cross = (icon_size as f32 + 8.0).min(cross - 2.0).max(20.0);
            let rect = if horizontal {
                Rect {
                    x: main,
                    y: (cross - cell_cross) * 0.5,
                    width: cell_length,
                    height: cell_cross,
                }
            } else {
                Rect {
                    x: (cross - cell_cross) * 0.5,
                    y: main,
                    width: cell_cross,
                    height: cell_length,
                }
            };
            self.draw_bar_workspace_background(pixmap, rect, workspace, bar, config);
            zones.push(HitZone {
                rect,
                target: HitTarget::Workspace {
                    workspace_id: workspace.id,
                },
            });

            let windows: Vec<_> = workspace.windows().collect();
            let label = workspace.index.to_string();
            let label_width = if bar.show_workspace_labels {
                self.text.measure(&label, label_font) + 7.0
            } else {
                0.0
            };
            let icon_start = 6.0 + label_width;
            for (window_index, window) in windows.into_iter().enumerate() {
                let offset = icon_start + window_index as f32 * (icon_size as f32 + 4.0);
                let icon_rect = if horizontal {
                    Rect {
                        x: rect.x + offset,
                        y: rect.y + (rect.height - icon_size as f32) * 0.5,
                        width: icon_size as f32,
                        height: icon_size as f32,
                    }
                } else {
                    Rect {
                        x: rect.x + (rect.width - icon_size as f32) * 0.5,
                        y: rect.y + offset,
                        width: icon_size as f32,
                        height: icon_size as f32,
                    }
                };
                self.draw_window_icon(pixmap, icon_rect, window, config, 0.0, 1.0);
                zones.push(HitZone {
                    rect: icon_rect,
                    target: HitTarget::Window {
                        workspace_id: workspace.id,
                        window_id: window.id,
                        rect: icon_rect,
                        canvas: None,
                    },
                });
            }

            if bar.show_workspace_labels {
                let color = if workspace.active || workspace.focused {
                    config.style.text.get()
                } else {
                    config.style.muted.get()
                };
                let (label_x, label_y) = if horizontal {
                    (rect.x + 6.0, rect.y + (rect.height - label_font) * 0.5)
                } else {
                    (
                        rect.x + (rect.width - self.text.measure(&label, label_font)) * 0.5,
                        rect.y + 6.0,
                    )
                };
                self.text.draw(
                    pixmap,
                    &label,
                    label_x * self.render_scale,
                    label_y * self.render_scale,
                    label_font * self.render_scale,
                    color,
                );
            }
            main += cell_length + gap;
        }
    }

    /// Draw the popup, applying the open/close transition.
    ///
    /// While the transition is running the panel is composed through an
    /// offscreen layer so it can be scaled and faded as a whole; once it has
    /// settled it is drawn straight into the surface.
    #[allow(clippy::too_many_arguments)]
    fn render_popup_layer(
        &mut self,
        pixmap: &mut Pixmap,
        layer: &mut Option<Pixmap>,
        host_output: &str,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        progress: f32,
        zones: &mut Vec<HitZone>,
    ) -> Option<Rect> {
        if progress >= 0.996 {
            return self.render_popup(pixmap, host_output, snapshot, config, selection, zones);
        }
        // Draw once into the reusable surface canvas, then copy only the
        // panel-sized region into the animation layer. Scaling a full-monitor
        // transparent pixmap was the dominant opening-frame cost.
        let panel = self.render_popup(pixmap, host_output, snapshot, config, selection, zones)?;
        let padding = config.style.shadow_spread + config.style.shadow_offset.abs() + 4.0;
        let s = self.render_scale;
        let device_panel = Rect {
            x: panel.x * s,
            y: panel.y * s,
            width: panel.width * s,
            height: panel.height * s,
        };
        let damage = damage_for_rect(device_panel, padding * s, pixmap.width(), pixmap.height())?;
        let reusable = layer
            .as_ref()
            .is_some_and(|layer| layer.width() == damage.width && layer.height() == damage.height);
        if !reusable {
            *layer = Pixmap::new(damage.width, damage.height);
        }
        let mut animation_layer = layer.take()?;
        copy_damage(pixmap, &mut animation_layer, damage);
        clear_damage(pixmap, damage);
        // Grow into place from slightly small, pivoting on the panel centre.
        let scale = lerp(config.animation.open_scale, 1.0, progress);
        let pivot_x = panel.x + panel.width * 0.5;
        let pivot_y = panel.y + panel.height * 0.5;
        let transform = Transform::from_translate(pivot_x, pivot_y)
            .pre_scale(scale, scale)
            .pre_translate(-pivot_x, -pivot_y);
        let paint = PixmapPaint {
            opacity: progress.clamp(0.0, 1.0),
            quality: FilterQuality::Bilinear,
            ..PixmapPaint::default()
        };
        pixmap.draw_pixmap(
            damage.x as i32,
            damage.y as i32,
            animation_layer.as_ref(),
            &paint,
            transform,
            None,
        );
        *layer = Some(animation_layer);
        Some(panel)
    }

    /// Lay out one canvas per output, side by side, and return the panel rect.
    fn render_popup(
        &mut self,
        pixmap: &mut Pixmap,
        host_output: &str,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) -> Option<Rect> {
        let outputs = self.popup_outputs(host_output, snapshot, config);
        if outputs.is_empty() {
            return None;
        }

        let padding = config.style.panel_padding;
        let gap = config.style.canvas_gap;
        let header = if config.popup.show_labels {
            config.style.label_font_size + 7.0
        } else {
            0.0
        };
        let count = outputs.len() as f32;
        let logical_width = pixmap.width() as f32 / self.render_scale;
        let logical_height = pixmap.height() as f32 / self.render_scale;
        let output_width = logical_width;
        let output_height = logical_height;
        // A configured canvas can be larger than the panel maximum. Fit the
        // canvas inside the final panel so its focal point stays centred and
        // content never extends beyond the panel chrome.
        let max_panel_width = config
            .popup
            .width
            .resolve(output_width)
            .min(logical_width - 24.0)
            .max(1.0);
        let max_panel_height = config
            .popup
            .height
            .resolve(output_height)
            .min(logical_height - 24.0)
            .max(1.0);
        let canvas_width_limit =
            ((max_panel_width - padding * 2.0 - gap * (count - 1.0)) / count).max(1.0);
        let canvas_height_limit = (max_panel_height - padding * 2.0 - header).max(1.0);
        let output_canvas_width_limit = ((logical_width - 64.0) / count).max(1.0);
        let output_canvas_height_limit = logical_height - 64.0;
        let canvas_width = config
            .popup
            .canvas_width
            .resolve(output_width)
            .max(120.0)
            .min(output_canvas_width_limit)
            .min(canvas_width_limit);
        let canvas_height = config
            .popup
            .canvas_height
            .resolve(output_height)
            .max(90.0)
            .min(output_canvas_height_limit)
            .min(canvas_height_limit);
        let panel_width = padding * 2.0 + canvas_width * count + gap * (count - 1.0);
        let panel_height = padding * 2.0 + header + canvas_height;

        let (mut panel_x, mut panel_y) = match config.popup.position {
            PopupPosition::Center => (
                (logical_width - panel_width) * 0.5,
                (logical_height - panel_height) * 0.5,
            ),
            PopupPosition::Fixed => (
                config.popup.fixed_x.resolve(output_width),
                config.popup.fixed_y.resolve(output_height),
            ),
            PopupPosition::Cursor => self
                .popup_origins
                .get(host_output)
                .copied()
                .map(|(x, y)| (x - panel_width * 0.5, y + 18.0))
                .unwrap_or(((logical_width - panel_width) * 0.5, 80.0)),
            PopupPosition::Anchored => {
                let placement = config.popup.placement(host_output);
                let free_x = logical_width - panel_width;
                let free_y = logical_height - panel_height;
                let margin_x = placement.margin.resolve(output_width);
                let margin_y = placement.margin.resolve(output_height);
                (
                    match placement.x {
                        AnchorX::Left => margin_x,
                        AnchorX::Center => free_x * 0.5,
                        AnchorX::Right => free_x - margin_x,
                    },
                    match placement.y {
                        AnchorY::Top => margin_y,
                        AnchorY::Center => free_y * 0.5,
                        AnchorY::Bottom => free_y - margin_y,
                    },
                )
            }
        };
        panel_x = panel_x.clamp(12.0, (logical_width - panel_width - 12.0).max(12.0));
        panel_y = panel_y.clamp(12.0, (logical_height - panel_height - 12.0).max(12.0));
        let panel = Rect {
            x: panel_x,
            y: panel_y,
            width: panel_width,
            height: panel_height,
        };

        draw_panel_chrome(
            pixmap,
            panel,
            config,
            self.panel_input_active,
            self.render_scale,
        );
        zones.push(HitZone {
            rect: panel,
            target: HitTarget::Panel,
        });

        for (index, output) in outputs.iter().enumerate() {
            let origin_x = panel_x + padding + index as f32 * (canvas_width + gap);
            let origin_y = panel_y + padding + header;
            if index > 0 {
                fill_rect(
                    pixmap,
                    (origin_x - gap * 0.5).round(),
                    origin_y + 6.0,
                    1.0,
                    (canvas_height - 12.0).max(0.0),
                    config.style.border_soft.get(),
                    self.render_scale,
                );
            }
            if config.popup.show_labels {
                self.text.draw(
                    pixmap,
                    &output.name,
                    (origin_x + 4.0) * self.render_scale,
                    (panel_y + padding) * self.render_scale,
                    config.style.label_font_size * self.render_scale,
                    config.style.muted.get(),
                );
            }
            self.draw_output_canvas(
                pixmap,
                output,
                Rect {
                    x: origin_x,
                    y: origin_y,
                    width: canvas_width,
                    height: canvas_height,
                },
                config,
                selection,
                zones,
            );
        }
        Some(panel)
    }

    /// Outputs shown in the popup: every monitor in combined mode, otherwise
    /// just the one hosting the surface.
    fn popup_outputs<'a>(
        &self,
        host_output: &str,
        snapshot: &'a Snapshot,
        config: &Config,
    ) -> Vec<&'a OutputModel> {
        if config.popup.outputs == PopupOutputs::Combined {
            snapshot.outputs.values().collect()
        } else {
            snapshot.outputs.get(host_output).into_iter().collect()
        }
    }

    /// Draw one monitor's cross into `canvas`.
    ///
    /// The focal point is fixed at the centre of the canvas: workspaces run
    /// vertically through it and the selected workspace's windows run
    /// horizontally through it. Changing the selection scrolls the content
    /// rather than moving a cursor, so the eye never has to track the
    /// selection across the panel.
    #[allow(clippy::too_many_arguments)]
    fn draw_output_canvas(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        canvas: Rect,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) {
        let metrics = IconMetrics::new(config);
        let focal_x = canvas.x + canvas.width * 0.5;
        let focal_y = canvas.y + canvas.height * 0.5;
        let scroll_y = self.anim.column_scroll(&output.name);
        let workspace_step = metrics.workspace_step(output);

        for (index, workspace) in output.workspaces.iter().enumerate() {
            let row_y = focal_y + index as f32 * workspace_step - scroll_y;
            if row_y < canvas.y - workspace_step
                || row_y > canvas.y + canvas.height + workspace_step
            {
                continue;
            }
            let emphasis = self.anim.workspace(workspace.id);
            let row = Rect {
                x: canvas.x,
                y: row_y - workspace_step * 0.5,
                width: canvas.width,
                height: workspace_step,
            };
            zones.push(HitZone {
                rect: clamp_rect(row, canvas),
                target: HitTarget::Workspace {
                    workspace_id: workspace.id,
                },
            });
            zones.push(HitZone {
                rect: clamp_rect(row, canvas),
                target: HitTarget::Canvas {
                    workspace_id: workspace.id,
                    rect: canvas,
                },
            });

            // Every row reads its own animated offset, selected or not, so the
            // focal column always reads as "what you get if you go here".
            let offset = self.anim.row_scroll(workspace.id);
            let stack_offset = self.anim.stack_scroll(workspace.id);
            self.draw_canvas_row(
                pixmap,
                output,
                workspace,
                canvas,
                focal_x - offset,
                row_y - stack_offset,
                emphasis,
                config,
                selection,
                zones,
            );
        }
    }

    /// One workspace's windows, laid out rightward from `start_x`.
    #[allow(clippy::too_many_arguments)]
    fn draw_canvas_row(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        workspace: &WorkspaceModel,
        canvas: Rect,
        start_x: f32,
        row_y: f32,
        emphasis: f32,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) {
        let metrics = IconMetrics::new(config);
        let selected_window = selection
            .filter(|selection| selection.output == output.name)
            .and_then(|selection| selection.window_id);
        let base_alpha = lerp(config.style.dim_alpha.clamp(0.0, 1.0), 1.0, emphasis);

        if workspace.windows().next().is_none() {
            // An empty workspace still occupies a rung on the vertical axis.
            let mut dot = config.style.border_soft.get();
            dot.set_alpha(dot.alpha() * base_alpha);
            fill_circle(
                pixmap,
                start_x,
                row_y,
                config.style.empty_dot_radius,
                dot,
                self.render_scale,
            );
            return;
        }

        for (column, windows) in workspace.columns.values().enumerate() {
            let center_x = start_x + column as f32 * metrics.column_step;
            for (stack_row, window) in windows.values().enumerate() {
                let center_y = row_y + stack_row as f32 * metrics.stack_step;
                self.draw_canvas_window(
                    pixmap,
                    workspace,
                    window,
                    canvas,
                    center_x,
                    center_y,
                    base_alpha,
                    selected_window,
                    metrics,
                    config,
                    zones,
                );
            }
        }

        let floating_start = workspace.columns.len();
        for (index, window) in workspace.floating.iter().enumerate() {
            self.draw_canvas_window(
                pixmap,
                workspace,
                window,
                canvas,
                start_x + (floating_start + index) as f32 * metrics.column_step,
                row_y,
                base_alpha,
                selected_window,
                metrics,
                config,
                zones,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_canvas_window(
        &mut self,
        pixmap: &mut Pixmap,
        workspace: &WorkspaceModel,
        window: &WindowModel,
        canvas: Rect,
        center_x: f32,
        center_y: f32,
        base_alpha: f32,
        selected_window: Option<u64>,
        metrics: IconMetrics,
        config: &Config,
        zones: &mut Vec<HitZone>,
    ) {
        let focus = self.anim.window(window.id);
        let size = lerp(metrics.base, metrics.focused, focus);
        let rect = Rect {
            x: center_x - size * 0.5,
            y: center_y - size * 0.5,
            width: size,
            height: size,
        };
        // Fade rather than clip at the canvas edge, so a partially visible
        // icon reads as "more this way" instead of a cut-off box.
        let alpha =
            base_alpha * edge_fade(rect, canvas, metrics.column_step.max(metrics.stack_step));
        if alpha <= 0.01 {
            return;
        }
        if window.floating {
            self.draw_floating_backing(pixmap, rect, config, alpha);
        }
        self.draw_window_icon(pixmap, rect, window, config, focus, alpha);
        if let Some(hit_rect) = visible_hit_rect(rect, canvas) {
            zones.push(HitZone {
                rect: hit_rect,
                target: HitTarget::Window {
                    workspace_id: workspace.id,
                    window_id: window.id,
                    rect,
                    canvas: Some(canvas),
                },
            });
        }
        let stacked_titles =
            config.popup.show_titles && !window.floating && workspace.max_stack() > 1;
        if stacked_titles {
            // A vertical column needs a label on every row. A label below the
            // icon only describes the selected row and overlaps the next icon
            // on compact portrait-output canvases.
            let label = if window.title.trim().is_empty() {
                self.icons
                    .app_label(&window.app_id)
                    .unwrap_or_else(|| window.app_id.clone())
            } else {
                window.title.clone()
            };
            let size = config.style.title_font_size;
            let x = rect.x + rect.width + 5.0;
            let title = truncate_label_to_width(
                &mut self.text,
                &label,
                (canvas.x + canvas.width - x).max(0.0),
                size,
            );
            let mut color = config.style.text.get();
            color.set_alpha(color.alpha() * alpha);
            self.text.draw(
                pixmap,
                &title,
                x * self.render_scale,
                (center_y - size * 0.42) * self.render_scale,
                size * self.render_scale,
                color,
            );
        } else if config.popup.show_titles && selected_window == Some(window.id) {
            let label = self
                .icons
                .app_label(&window.app_id)
                .unwrap_or_else(|| window.title.clone());
            let title = truncate_label(&label, 18);
            let size = config.style.title_font_size;
            let width = self.text.measure(&title, size);
            let mut color = config.style.text.get();
            color.set_alpha(color.alpha() * focus);
            self.text.draw(
                pixmap,
                &title,
                (center_x - width * 0.5) * self.render_scale,
                ((rect.y + rect.height + 4.0).min(canvas.y + canvas.height - size - 1.0))
                    * self.render_scale,
                size * self.render_scale,
                color,
            );
        }
    }

    fn draw_bar_workspace_background(
        &mut self,
        pixmap: &mut Pixmap,
        rect: Rect,
        workspace: &WorkspaceModel,
        bar: &BarConfig,
        config: &Config,
    ) {
        let hovered = self.workspace_hovered(workspace.id);
        if bar.show_capsule {
            fill_rounded_rect(
                pixmap,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                config.style.radius,
                if workspace.active || workspace.focused || hovered {
                    config.style.surface_hover.get()
                } else {
                    config.style.surface.get()
                },
                self.render_scale,
            );
            stroke_rounded_rect(
                pixmap,
                rect.x + 0.5,
                rect.y + 0.5,
                rect.width - 1.0,
                rect.height - 1.0,
                config.style.radius,
                if workspace.urgent {
                    config.style.urgent.get()
                } else if workspace.active || workspace.focused {
                    config.style.selection_border.get()
                } else {
                    config.style.border_soft.get()
                },
                if workspace.active || workspace.focused {
                    1.25
                } else {
                    config.style.border_width
                },
                self.render_scale,
            );
        }
        if bar.show_active_indicator && (workspace.active || workspace.focused || workspace.urgent)
        {
            let diameter = 4.0;
            fill_circle(
                pixmap,
                rect.x + rect.width * 0.5,
                rect.y + rect.height - 2.5,
                diameter * 0.5,
                if workspace.urgent {
                    config.style.urgent.get()
                } else {
                    config.style.accent.get()
                },
                self.render_scale,
            );
        }
    }

    /// Dark disc behind a floating window icon so it stays readable over the
    /// panel gradient.
    fn draw_floating_backing(
        &mut self,
        pixmap: &mut Pixmap,
        rect: Rect,
        config: &Config,
        alpha: f32,
    ) {
        let mut color = config.style.floating_backing.get();
        color.set_alpha(color.alpha() * alpha.clamp(0.0, 1.0));
        fill_circle(
            pixmap,
            rect.x + rect.width * 0.5,
            rect.y + rect.height * 0.5,
            rect.width * 0.56,
            color,
            self.render_scale,
        );
    }

    /// Draw an app icon at `rect`, dimmed to `alpha`.
    ///
    /// Selection is conveyed by the fixed focal position, scale, and opacity.
    fn draw_window_icon(
        &mut self,
        pixmap: &mut Pixmap,
        rect: Rect,
        window: &WindowModel,
        config: &Config,
        emphasis: f32,
        alpha: f32,
    ) {
        // Focus has no halo: position, scale, and opacity show selection.
        // Urgency has a separate configurable indicator.
        let emphasis = emphasis.clamp(0.0, 1.0);
        let urgent_badge = window.urgent && config.style.urgent_indicator == UrgentIndicator::Badge;
        if window.urgent && !urgent_badge {
            self.draw_urgent_indicator(pixmap, rect, config);
        }
        // Emphasis lifts an icon back to full opacity as it eases into focus.
        let alpha = lerp(alpha.clamp(0.0, 1.0), 1.0, emphasis);
        if let Some((width, height, pixels)) = self.icons.get(&window.app_id) {
            let icon_size = rect.width.min(rect.height).round() * self.render_scale;
            blit_icon_alpha(
                pixmap,
                (rect.x * self.render_scale).round() as i32,
                (rect.y * self.render_scale).round() as i32,
                icon_size.round() as u32,
                pixels,
                *width,
                *height,
                alpha,
            );
        } else {
            let mut plate = config.style.border.get();
            plate.set_alpha(plate.alpha() * alpha);
            fill_rounded_rect(
                pixmap,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                rect.width * 0.22,
                plate,
                self.render_scale,
            );
            let fallback = window
                .app_id
                .chars()
                .find(|character| character.is_alphanumeric())
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            let mut text_color = config.style.text.get();
            text_color.set_alpha(text_color.alpha() * alpha);
            let font_size = rect.height * 0.55;
            let text_width = self.text.measure(&fallback, font_size);
            self.text.draw(
                pixmap,
                &fallback,
                (rect.x + (rect.width - text_width) * 0.5) * self.render_scale,
                (rect.y + rect.height * 0.2) * self.render_scale,
                font_size * self.render_scale,
                text_color,
            );
        }
        if urgent_badge {
            self.draw_urgent_indicator(pixmap, rect, config);
        }
    }

    fn draw_urgent_indicator(&self, pixmap: &mut Pixmap, rect: Rect, config: &Config) {
        let urgent = config.style.urgent.get();
        match config.style.urgent_indicator {
            UrgentIndicator::None => {}
            UrgentIndicator::Ring => {
                let width = config.style.urgent_ring_width.max(0.0);
                let padding = config.style.urgent_ring_padding.max(0.0);
                if width > 0.0 {
                    stroke_rounded_rect(
                        pixmap,
                        rect.x - padding,
                        rect.y - padding,
                        rect.width + padding * 2.0,
                        rect.height + padding * 2.0,
                        config.style.radius + padding,
                        urgent,
                        width,
                        self.render_scale,
                    );
                }
            }
            UrgentIndicator::Glow => {
                let spread = config.style.urgent_glow_spread.max(0.0);
                let alpha = config.style.urgent_glow_alpha.clamp(0.0, 1.0);
                if spread <= 0.0 || alpha <= 0.0 {
                    return;
                }
                let mut color = urgent;
                color.set_alpha(color.alpha() * alpha);
                fill_radial_glow(
                    pixmap,
                    rect.x + rect.width * 0.5,
                    rect.y + rect.height * 0.5,
                    rect.width.max(rect.height) * 0.5 + spread,
                    color,
                    self.render_scale,
                );
            }
            UrgentIndicator::Badge => {
                let radius = config.style.urgent_badge_radius.max(0.0);
                if radius <= 0.0 {
                    return;
                }
                let x = rect.x + rect.width - radius * 0.15;
                let y = rect.y + radius * 0.15;
                let mut backing = config.style.background_end.get();
                backing.set_alpha(backing.alpha().max(0.82));
                fill_circle(pixmap, x, y, radius + 1.25, backing, self.render_scale);
                fill_circle(pixmap, x, y, radius, urgent, self.render_scale);
            }
        }
    }

    fn render_drag_ghost(
        &mut self,
        pixmap: &mut Pixmap,
        surface: &SurfaceInfo,
        drag: &DragVisual,
        config: &Config,
    ) -> Option<Rect> {
        let x = drag.global_x - surface.global_x;
        let y = drag.global_y - surface.global_y;
        if x < -48.0
            || y < -48.0
            || x > surface.width as f64 + 48.0
            || y > surface.height as f64 + 48.0
        {
            return None;
        }
        let size = config.style.icon_size.max(28) as f32;
        let rect = Rect {
            x: x as f32 - size * 0.5,
            y: y as f32 - size * 0.5,
            width: size,
            height: size,
        };
        fill_rounded_rect(
            pixmap,
            rect.x - 5.0,
            rect.y - 5.0,
            rect.width + 10.0,
            rect.height + 10.0,
            config.style.radius,
            config.style.floating_backing.get(),
            self.render_scale,
        );
        let ghost_window = WindowModel {
            id: drag.window_id,
            workspace_id: 0,
            app_id: drag.app_id.clone(),
            title: String::new(),
            focused: false,
            urgent: false,
            floating: false,
            column: None,
            tile_width: 0.0,
            tile_height: 0.0,
            x: None,
            y: None,
        };
        self.draw_window_icon(pixmap, rect, &ghost_window, config, 0.0, 1.0);
        Some(Rect {
            x: rect.x - 5.0,
            y: rect.y - 5.0,
            width: rect.width + 10.0,
            height: rect.height + 10.0,
        })
    }
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    let mut chars = label.chars();
    let mut truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        truncated.push('…');
    }
    truncated
}

/// Truncate a title to the space available beside a vertically stacked icon.
/// Text is measured with the active font because character count alone does
/// not keep wide glyphs inside a compact portrait canvas.
fn truncate_label_to_width(
    text: &mut TextRenderer,
    label: &str,
    max_width: f32,
    size: f32,
) -> String {
    if max_width <= 0.0 || label.is_empty() {
        return String::new();
    }
    if text.measure(label, size) <= max_width {
        return label.to_string();
    }
    const ELLIPSIS: &str = "…";
    if text.measure(ELLIPSIS, size) > max_width {
        return String::new();
    }

    let mut boundaries: Vec<usize> = label.char_indices().map(|(at, _)| at).collect();
    boundaries.push(label.len());
    let mut candidate = String::with_capacity(label.len() + ELLIPSIS.len());
    let mut fits = 0usize;
    let mut over = boundaries.len() - 1;
    while over - fits > 1 {
        let middle = fits + (over - fits) / 2;
        candidate.clear();
        candidate.push_str(&label[..boundaries[middle]]);
        candidate.push_str(ELLIPSIS);
        if text.measure(&candidate, size) <= max_width {
            fits = middle;
        } else {
            over = middle;
        }
    }

    if fits == 0 {
        ELLIPSIS.into()
    } else {
        let mut result = String::with_capacity(boundaries[fits] + ELLIPSIS.len());
        result.push_str(&label[..boundaries[fits]]);
        result.push_str(ELLIPSIS);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partially_visible_icon_keeps_visible_hit_area() {
        let canvas = Rect {
            x: 10.0,
            y: 10.0,
            width: 100.0,
            height: 100.0,
        };
        let icon = Rect {
            x: 95.0,
            y: 40.0,
            width: 30.0,
            height: 30.0,
        };

        let hit = visible_hit_rect(icon, canvas).unwrap();
        assert_eq!(hit.x, 95.0);
        assert_eq!(hit.width, 15.0);
        assert!(hit.contains(109.0, 55.0));
        assert!(!hit.contains(110.0, 55.0));
    }
    use crate::model::OutputModel;
    use std::collections::BTreeMap;

    fn snapshot() -> Snapshot {
        Snapshot {
            outputs: BTreeMap::from([
                (
                    "eDP-1".into(),
                    OutputModel {
                        name: "eDP-1".into(),
                        x: 0,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        workspaces: Vec::new(),
                    },
                ),
                (
                    "HDMI-A-1".into(),
                    OutputModel {
                        name: "HDMI-A-1".into(),
                        x: 1920,
                        y: 0,
                        width: 2560,
                        height: 1440,
                        workspaces: Vec::new(),
                    },
                ),
            ]),
            generation: 7,
            ..Snapshot::default()
        }
    }

    fn bar_surface(id: usize, index: usize, output: &str, width: u32) -> SurfaceInfo {
        SurfaceInfo {
            id,
            kind: SurfaceKind::Bar(index),
            output: output.into(),
            global_x: 0.0,
            global_y: 0.0,
            width,
            height: 38,
            configured: true,
            frame_ready: true,
            needs_full_redraw: false,
            scale_factor: 1,
        }
    }

    #[test]
    fn unchanged_bar_is_not_redrawn_for_unrelated_dirty_frames() {
        let mut renderer = Renderer::new();
        let config = Config {
            bars: vec![BarConfig {
                output: "eDP-1".into(),
                ..BarConfig::default()
            }],
            ..Config::default()
        };
        let snapshot = snapshot();
        let surface = bar_surface(1, 0, "eDP-1", 420);

        assert!(
            renderer
                .render(&surface, &snapshot, &config, None, None)
                .is_some()
        );
        assert!(
            renderer
                .render(&surface, &snapshot, &config, None, None)
                .is_none()
        );
    }

    #[test]
    fn scale_invalidation_bypasses_unchanged_bar_cache() {
        let mut renderer = Renderer::new();
        let config = Config {
            bars: vec![BarConfig {
                output: "eDP-1".into(),
                ..BarConfig::default()
            }],
            ..Config::default()
        };
        let snapshot = snapshot();
        let mut surface = bar_surface(1, 0, "eDP-1", 420);

        assert!(
            renderer
                .render(&surface, &snapshot, &config, None, None)
                .is_some()
        );
        surface.needs_full_redraw = true;
        assert!(
            renderer
                .render(&surface, &snapshot, &config, None, None)
                .is_some(),
            "scale invalidation must redraw an otherwise unchanged bar"
        );
    }

    #[test]
    fn each_surface_keeps_its_own_sized_canvas() {
        let mut renderer = Renderer::new();
        let config = Config {
            bars: vec![
                BarConfig {
                    output: "eDP-1".into(),
                    ..BarConfig::default()
                },
                BarConfig {
                    output: "HDMI-A-1".into(),
                    ..BarConfig::default()
                },
            ],
            ..Config::default()
        };
        let snapshot = snapshot();
        let laptop = bar_surface(1, 0, "eDP-1", 420);
        let desktop = bar_surface(2, 1, "HDMI-A-1", 640);

        assert!(
            renderer
                .render(&laptop, &snapshot, &config, None, None)
                .is_some()
        );
        assert!(
            renderer
                .render(&desktop, &snapshot, &config, None, None)
                .is_some()
        );

        assert_eq!(renderer.surfaces[&1].canvas.as_ref().unwrap().width(), 420);
        assert_eq!(renderer.surfaces[&2].canvas.as_ref().unwrap().width(), 640);
    }

    #[test]
    fn hidden_surface_releases_its_render_buffers() {
        let mut renderer = Renderer::new();
        let config = Config {
            bars: vec![BarConfig {
                output: "eDP-1".into(),
                ..BarConfig::default()
            }],
            ..Config::default()
        };
        let snapshot = snapshot();
        let surface = bar_surface(1, 0, "eDP-1", 420);

        assert!(
            renderer
                .render(&surface, &snapshot, &config, None, None)
                .is_some()
        );
        renderer.retain_surfaces(&[]);

        assert!(renderer.surfaces.is_empty());
        assert!(renderer.hit_zones.is_empty());
    }

    /// Truncation must still produce the longest prefix that fits, and must
    /// not measure every prefix on the way there.
    #[test]
    fn truncation_keeps_the_longest_prefix_that_fits() {
        let mut text = TextRenderer::new();
        let label = "a very long window title that will not fit";
        let full = text.measure(label, 14.0);

        // Room for everything: unchanged.
        assert_eq!(
            truncate_label_to_width(&mut text, label, full + 10.0, 14.0),
            label
        );
        // No room at all.
        assert_eq!(truncate_label_to_width(&mut text, label, 0.0, 14.0), "");

        let clipped = truncate_label_to_width(&mut text, label, full / 2.0, 14.0);
        assert!(
            clipped.ends_with('…'),
            "clipped label must be marked: {clipped:?}"
        );
        assert!(
            text.measure(&clipped, 14.0) <= full / 2.0,
            "clipped label must actually fit"
        );
        assert!(clipped.chars().count() < label.chars().count());
    }
}
