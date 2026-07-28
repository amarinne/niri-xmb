use crate::anim::{Animator, Timing, lerp};
use crate::config::{AnchorX, AnchorY, BarConfig, Config, Edge, PopupOutputs, PopupPosition};
use crate::draw::{
    blit_icon_alpha, clear, fill_circle, fill_gradient_rounded_rect, fill_rect, fill_rounded_rect,
    hex_color, stroke_rounded_rect,
};
use crate::icons::IconCache;
use crate::model::{OutputModel, Snapshot, WindowModel, WindowSlot, WorkspaceModel};
use crate::text::TextRenderer;
use crate::wayland::{SurfaceInfo, SurfaceKind};
use std::collections::HashMap;
use tiny_skia::{Color, FilterQuality, Pixmap, PixmapPaint, Transform};

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

/// Whether a rect is far enough inside the canvas to be worth hit-testing.
fn rect_within(rect: Rect, canvas: Rect) -> bool {
    rect.x >= canvas.x - 1.0
        && rect.y >= canvas.y - 1.0
        && rect.x + rect.width <= canvas.x + canvas.width + 1.0
        && rect.y + rect.height <= canvas.y + canvas.height + 1.0
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

/// Panel background: soft drop shadow, vertical gradient body, hairline edge.
fn draw_panel_chrome(pixmap: &mut Pixmap, panel: Rect, config: &Config) {
    let radius = config.style.panel_radius;
    let offset = config.style.shadow_offset;
    let spread = config.style.shadow_spread;
    let shadow = hex_color(&config.style.shadow);
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
    );
    fill_rounded_rect(
        pixmap,
        panel.x,
        panel.y + offset,
        panel.width,
        panel.height,
        radius,
        shadow,
    );
    fill_gradient_rounded_rect(
        pixmap,
        panel.x,
        panel.y,
        panel.width,
        panel.height,
        radius,
        hex_color(&config.style.background),
        hex_color(&config.style.background_end),
    );
    stroke_rounded_rect(
        pixmap,
        panel.x + 0.5,
        panel.y + 0.5,
        panel.width - 1.0,
        panel.height - 1.0,
        radius,
        hex_color(&config.style.border),
        config.style.border_width,
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

#[derive(Debug, Clone)]
pub enum HitTarget {
    Panel,
    Workspace {
        workspace_id: u64,
        column_count: usize,
    },
    Canvas {
        output: String,
        workspace_id: u64,
        rect: Rect,
    },
    Window {
        output: String,
        workspace_id: u64,
        window_id: u64,
        column: Option<usize>,
        floating: bool,
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
    /// Staged cursor position per workspace id, for workspaces the popup
    /// session has already visited. Absent entries fall back to the
    /// workspace's own active window.
    pub anchors: HashMap<u64, usize>,
}

impl SelectionVisual {
    /// Index that should sit on the focal column for `workspace`.
    pub fn anchor(&self, workspace: &WorkspaceModel) -> usize {
        let count = workspace.windows().count();
        match self.anchors.get(&workspace.id) {
            Some(&staged) if staged < count => staged,
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
    anim: Animator,
    /// Scratch buffers reused across frames. A popup surface is output-sized,
    /// so reallocating these per frame is the dominant cost of animating.
    canvas: Option<Pixmap>,
    layer: Option<Pixmap>,
    /// Surfaces already presented blank, so they can be skipped.
    blanked: std::collections::HashSet<usize>,
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
            anim: Animator::new(),
            canvas: None,
            layer: None,
            blanked: std::collections::HashSet::new(),
        }
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
    ) -> Option<&[u8]> {
        let (width, height) = (surface.width.max(1), surface.height.max(1));
        let progress = match surface.kind {
            SurfaceKind::Bar(_) => 1.0,
            SurfaceKind::Popup => self.anim.panel(&surface.output),
        };
        let blank = progress <= 0.004 && drag.is_none();
        if blank && !self.blanked.insert(surface.id) {
            // Already cleared on a previous frame; nothing to upload.
            return None;
        }
        if !blank {
            self.blanked.remove(&surface.id);
        }

        let reusable = self
            .canvas
            .as_ref()
            .is_some_and(|pixmap| pixmap.width() == width && pixmap.height() == height);
        if !reusable {
            self.canvas = Pixmap::new(width, height);
        }
        let mut pixmap = self.canvas.take()?;
        clear(&mut pixmap, Color::TRANSPARENT);
        self.render_surface = Some(surface.id);
        let mut zones = Vec::new();

        match surface.kind {
            SurfaceKind::Bar(index) => {
                if let (Some(bar), Some(output)) = (
                    config.bars.get(index),
                    snapshot.outputs.get(&surface.output),
                ) {
                    self.render_bar(&mut pixmap, output, bar, config, &mut zones);
                }
            }
            SurfaceKind::Popup => {
                if progress > 0.004 {
                    self.render_popup_layer(
                        &mut pixmap,
                        &surface.output,
                        snapshot,
                        config,
                        selection,
                        progress,
                        &mut zones,
                    );
                }
            }
        }

        if let Some(drag) = drag {
            self.render_drag_ghost(&mut pixmap, surface, drag, config);
        }
        self.hit_zones.insert(surface.id, zones);
        self.canvas = Some(pixmap);
        self.canvas.as_ref().map(|pixmap| pixmap.data())
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
        let length = if horizontal {
            pixmap.width() as f32
        } else {
            pixmap.height() as f32
        };
        let cross = if horizontal {
            pixmap.height() as f32
        } else {
            pixmap.width() as f32
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
                    column_count: workspace.columns.len(),
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
                        output: output.name.clone(),
                        workspace_id: workspace.id,
                        window_id: window.id,
                        column: window.column,
                        floating: window.floating,
                        rect: icon_rect,
                        canvas: None,
                    },
                });
            }

            if bar.show_workspace_labels {
                let color = if workspace.active || workspace.focused {
                    hex_color(&config.style.text)
                } else {
                    hex_color(&config.style.muted)
                };
                let (label_x, label_y) = if horizontal {
                    (rect.x + 6.0, rect.y + (rect.height - label_font) * 0.5)
                } else {
                    (
                        rect.x + (rect.width - self.text.measure(&label, label_font)) * 0.5,
                        rect.y + 6.0,
                    )
                };
                self.text
                    .draw(pixmap, &label, label_x, label_y, label_font, color);
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
        host_output: &str,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        progress: f32,
        zones: &mut Vec<HitZone>,
    ) {
        if progress >= 0.996 {
            self.render_popup(pixmap, host_output, snapshot, config, selection, zones);
            return;
        }
        let reusable = self
            .layer
            .as_ref()
            .is_some_and(|l| l.width() == pixmap.width() && l.height() == pixmap.height());
        if !reusable {
            self.layer = Pixmap::new(pixmap.width(), pixmap.height());
        }
        let Some(mut layer) = self.layer.take() else {
            return;
        };
        clear(&mut layer, Color::TRANSPARENT);
        let panel = self.render_popup(&mut layer, host_output, snapshot, config, selection, zones);
        let Some(panel) = panel else {
            self.layer = Some(layer);
            return;
        };
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
        pixmap.draw_pixmap(0, 0, layer.as_ref(), &paint, transform, None);
        self.layer = Some(layer);
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
        // Canvases keep their configured size; `popup.width`/`height` only cap
        // how much of the panel can be shown on a small screen.
        let canvas_width = (config.popup.canvas_width.max(120) as f32)
            .min((pixmap.width().saturating_sub(64) as f32 / count).max(120.0));
        let canvas_height = (config.popup.canvas_height.max(90) as f32)
            .min(pixmap.height().saturating_sub(64) as f32);
        let panel_width = (padding * 2.0 + canvas_width * count + gap * (count - 1.0))
            .min(config.popup.width as f32)
            .min(pixmap.width().saturating_sub(24) as f32);
        let panel_height = (padding * 2.0 + header + canvas_height)
            .min(config.popup.height as f32)
            .min(pixmap.height().saturating_sub(24) as f32);

        let (mut panel_x, mut panel_y) = match config.popup.position {
            PopupPosition::Center => (
                (pixmap.width() as f32 - panel_width) * 0.5,
                (pixmap.height() as f32 - panel_height) * 0.5,
            ),
            PopupPosition::Fixed => (config.popup.fixed_x as f32, config.popup.fixed_y as f32),
            PopupPosition::Cursor => self
                .popup_origins
                .get(host_output)
                .copied()
                .map(|(x, y)| (x - panel_width * 0.5, y + 18.0))
                .unwrap_or(((pixmap.width() as f32 - panel_width) * 0.5, 80.0)),
            PopupPosition::Anchored => {
                let placement = config.popup.placement(host_output);
                let free_x = pixmap.width() as f32 - panel_width;
                let free_y = pixmap.height() as f32 - panel_height;
                (
                    match placement.x {
                        AnchorX::Left => placement.margin,
                        AnchorX::Center => free_x * 0.5,
                        AnchorX::Right => free_x - placement.margin,
                    },
                    match placement.y {
                        AnchorY::Top => placement.margin,
                        AnchorY::Center => free_y * 0.5,
                        AnchorY::Bottom => free_y - placement.margin,
                    },
                )
            }
        };
        panel_x = panel_x.clamp(12.0, (pixmap.width() as f32 - panel_width - 12.0).max(12.0));
        panel_y = panel_y.clamp(
            12.0,
            (pixmap.height() as f32 - panel_height - 12.0).max(12.0),
        );
        let panel = Rect {
            x: panel_x,
            y: panel_y,
            width: panel_width,
            height: panel_height,
        };

        draw_panel_chrome(pixmap, panel, config);
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
                    hex_color(&config.style.border_soft),
                );
            }
            if config.popup.show_labels {
                self.text.draw(
                    pixmap,
                    &output.name,
                    origin_x + 4.0,
                    panel_y + padding,
                    config.style.label_font_size,
                    hex_color(&config.style.muted),
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
                    column_count: workspace.columns.len(),
                },
            });
            zones.push(HitZone {
                rect: clamp_rect(row, canvas),
                target: HitTarget::Canvas {
                    output: output.name.clone(),
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
            let mut dot = hex_color(&config.style.border_soft);
            dot.set_alpha(dot.alpha() * base_alpha);
            fill_circle(pixmap, start_x, row_y, config.style.empty_dot_radius, dot);
            return;
        }

        for (column, windows) in workspace.columns.values().enumerate() {
            let center_x = start_x + column as f32 * metrics.column_step;
            for (stack_row, window) in windows.values().enumerate() {
                let center_y = row_y + stack_row as f32 * metrics.stack_step;
                self.draw_canvas_window(
                    pixmap,
                    output,
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
                output,
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
        output: &OutputModel,
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
        if rect_within(rect, canvas) {
            zones.push(HitZone {
                rect,
                target: HitTarget::Window {
                    output: output.name.clone(),
                    workspace_id: workspace.id,
                    window_id: window.id,
                    column: window.column,
                    floating: window.floating,
                    rect,
                    canvas: Some(canvas),
                },
            });
        }
        if config.popup.show_titles && selected_window == Some(window.id) {
            let label = self
                .icons
                .app_label(&window.app_id)
                .unwrap_or_else(|| window.title.clone());
            let title = truncate_label(&label, 18);
            let size = config.style.title_font_size;
            let width = self.text.measure(&title, size);
            let mut color = hex_color(&config.style.text);
            color.set_alpha(color.alpha() * focus);
            self.text.draw(
                pixmap,
                &title,
                center_x - width * 0.5,
                (rect.y + rect.height + 4.0).min(canvas.y + canvas.height - size - 1.0),
                size,
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
                    hex_color(&config.style.surface_hover)
                } else {
                    hex_color(&config.style.surface)
                },
            );
            stroke_rounded_rect(
                pixmap,
                rect.x + 0.5,
                rect.y + 0.5,
                rect.width - 1.0,
                rect.height - 1.0,
                config.style.radius,
                if workspace.urgent {
                    hex_color(&config.style.urgent)
                } else if workspace.active || workspace.focused {
                    hex_color(&config.style.selection_border)
                } else {
                    hex_color(&config.style.border_soft)
                },
                if workspace.active || workspace.focused {
                    1.25
                } else {
                    config.style.border_width
                },
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
                    hex_color(&config.style.urgent)
                } else {
                    hex_color(&config.style.accent)
                },
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
        let mut color = hex_color(&config.style.floating_backing);
        color.set_alpha(color.alpha() * alpha.clamp(0.0, 1.0));
        fill_circle(
            pixmap,
            rect.x + rect.width * 0.5,
            rect.y + rect.height * 0.5,
            rect.width * 0.56,
            color,
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
        // No halo: the selected icon is already the one sitting on the focal
        // point and drawn larger, so a glow only adds noise around it.
        let emphasis = emphasis.clamp(0.0, 1.0);
        // Urgency keeps a hard ring: it must survive dimming and read as an
        // alarm rather than as emphasis.
        if window.urgent {
            stroke_rounded_rect(
                pixmap,
                rect.x - 2.0,
                rect.y - 2.0,
                rect.width + 4.0,
                rect.height + 4.0,
                config.style.radius,
                hex_color(&config.style.urgent),
                1.25,
            );
        }
        // Emphasis lifts an icon back to full opacity as it eases into focus.
        let alpha = lerp(alpha.clamp(0.0, 1.0), 1.0, emphasis);
        if let Some((width, height, pixels)) = self.icons.get(&window.app_id) {
            blit_icon_alpha(
                pixmap,
                rect.x.round() as i32,
                rect.y.round() as i32,
                rect.width.min(rect.height).round() as u32,
                pixels,
                *width,
                *height,
                alpha,
            );
        } else {
            let mut plate = hex_color(&config.style.border);
            plate.set_alpha(plate.alpha() * alpha);
            fill_rounded_rect(
                pixmap,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                rect.width * 0.22,
                plate,
            );
            let fallback = window
                .app_id
                .chars()
                .find(|character| character.is_alphanumeric())
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            let mut text_color = hex_color(&config.style.text);
            text_color.set_alpha(text_color.alpha() * alpha);
            let font_size = rect.height * 0.55;
            let text_width = self.text.measure(&fallback, font_size);
            self.text.draw(
                pixmap,
                &fallback,
                rect.x + (rect.width - text_width) * 0.5,
                rect.y + rect.height * 0.2,
                font_size,
                text_color,
            );
        }
    }

    fn render_drag_ghost(
        &mut self,
        pixmap: &mut Pixmap,
        surface: &SurfaceInfo,
        drag: &DragVisual,
        config: &Config,
    ) {
        let x = drag.global_x - surface.global_x;
        let y = drag.global_y - surface.global_y;
        if x < -48.0
            || y < -48.0
            || x > surface.width as f64 + 48.0
            || y > surface.height as f64 + 48.0
        {
            return;
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
            hex_color(&config.style.floating_backing),
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
