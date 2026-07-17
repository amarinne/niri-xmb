use crate::config::{BarConfig, Config, Edge, PopupOutputs, PopupPosition};
use crate::icons::IconCache;
use crate::model::{OutputModel, Snapshot, WindowModel, WorkspaceModel};
use crate::wayland::{SurfaceInfo, SurfaceKind};
use std::collections::HashMap;
use tiny_skia::{Color, Pixmap};
use woven_common::{
    draw::{
        blit_icon, clear, fill_circle, fill_rect, fill_rounded_rect, hex_color, stroke_rounded_rect,
    },
    text::TextRenderer,
};

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
        }
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

    pub fn render(
        &mut self,
        surface: &SurfaceInfo,
        snapshot: &Snapshot,
        config: &Config,
        popup_visible: bool,
        drag: Option<&DragVisual>,
        selection: Option<&SelectionVisual>,
    ) -> Vec<u8> {
        let Some(mut pixmap) = Pixmap::new(surface.width.max(1), surface.height.max(1)) else {
            return Vec::new();
        };
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
            SurfaceKind::Popup if popup_visible => {
                self.render_popup(
                    &mut pixmap,
                    &surface.output,
                    snapshot,
                    config,
                    selection,
                    &mut zones,
                );
            }
            SurfaceKind::Popup => {}
        }

        if let Some(drag) = drag {
            self.render_drag_ghost(&mut pixmap, surface, drag, config);
        }
        self.hit_zones.insert(surface.id, zones);
        pixmap.take()
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
                self.draw_window_icon(pixmap, icon_rect, window, config, false);
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

    fn render_popup(
        &mut self,
        pixmap: &mut Pixmap,
        host_output: &str,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) {
        if config.popup.outputs == PopupOutputs::Combined {
            self.render_combined_popup(pixmap, host_output, snapshot, config, selection, zones);
        } else if let Some(output) = snapshot.outputs.get(host_output) {
            self.render_single_popup(pixmap, output, config, selection, zones);
        }
    }

    fn render_combined_popup(
        &mut self,
        pixmap: &mut Pixmap,
        host_output: &str,
        snapshot: &Snapshot,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) {
        if snapshot.outputs.is_empty() {
            return;
        }
        let outputs = snapshot.outputs.values().collect::<Vec<_>>();
        let icon_span = config.style.icon_size.max(28) as f32 + 14.0;
        let section_desired_widths = outputs
            .iter()
            .map(|output| {
                let widest_strip = output
                    .workspaces
                    .iter()
                    .map(|workspace| workspace.columns.len().max(1) as f32 * icon_span)
                    .fold(0.0, f32::max);
                (widest_strip + 64.0).max(220.0)
            })
            .collect::<Vec<_>>();
        let max_workspace_count = outputs
            .iter()
            .map(|output| output.workspaces.len())
            .max()
            .unwrap_or(1)
            .max(1);
        let max_stack = outputs
            .iter()
            .flat_map(|output| &output.workspaces)
            .flat_map(|workspace| workspace.columns.values())
            .map(|windows| windows.len())
            .max()
            .unwrap_or(1);
        let desired_row_height = (max_stack as f32 * 34.0 + 14.0).max(58.0);
        let padding = 16.0;
        let output_gap = 8.0;
        let header_height = 26.0;
        let desired_panel_width = padding * 2.0
            + section_desired_widths.iter().sum::<f32>()
            + output_gap * outputs.len().saturating_sub(1) as f32;
        let desired_panel_height =
            padding * 2.0 + header_height + desired_row_height * max_workspace_count as f32;
        let panel_width = desired_panel_width
            .min(config.popup.width as f32)
            .min(pixmap.width().saturating_sub(24) as f32)
            .max(360.0);
        let panel_height = desired_panel_height
            .min(config.popup.height as f32)
            .min(pixmap.height().saturating_sub(24) as f32)
            .max(180.0);
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
        };
        panel_x = panel_x.clamp(12.0, (pixmap.width() as f32 - panel_width - 12.0).max(12.0));
        panel_y = panel_y.clamp(
            12.0,
            (pixmap.height() as f32 - panel_height - 12.0).max(12.0),
        );

        fill_rounded_rect(
            pixmap,
            panel_x + 5.0,
            panel_y + 7.0,
            panel_width,
            panel_height,
            config.style.radius + 7.0,
            Color::from_rgba8(0, 0, 0, 82),
        );
        fill_rounded_rect(
            pixmap,
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            config.style.radius + 7.0,
            hex_color(&config.style.background),
        );
        stroke_rounded_rect(
            pixmap,
            panel_x + 0.5,
            panel_y + 0.5,
            panel_width - 1.0,
            panel_height - 1.0,
            config.style.radius + 7.0,
            hex_color(&config.style.border),
            config.style.border_width,
        );
        zones.push(HitZone {
            rect: Rect {
                x: panel_x,
                y: panel_y,
                width: panel_width,
                height: panel_height,
            },
            target: HitTarget::Panel,
        });
        let inner_width =
            panel_width - padding * 2.0 - output_gap * outputs.len().saturating_sub(1) as f32;
        let desired_sections_total = section_desired_widths.iter().sum::<f32>().max(1.0);
        let section_scale = (inner_width / desired_sections_total).min(1.0);
        let section_height = panel_height - padding * 2.0;
        let row_height = ((section_height - header_height) / max_workspace_count as f32).max(36.0);
        let mut section_x = panel_x + padding;

        for (output_index, output) in outputs.into_iter().enumerate() {
            let section_width = if output_index + 1 == section_desired_widths.len() {
                panel_x + panel_width - padding - section_x
            } else {
                section_desired_widths[output_index] * section_scale
            };
            let section = Rect {
                x: section_x,
                y: panel_y + padding,
                width: section_width,
                height: section_height,
            };
            fill_rounded_rect(
                pixmap,
                section.x,
                section.y,
                section.width,
                section.height,
                config.style.radius + 2.0,
                hex_color(&config.style.surface),
            );
            stroke_rounded_rect(
                pixmap,
                section.x + 0.5,
                section.y + 0.5,
                section.width - 1.0,
                section.height - 1.0,
                config.style.radius + 2.0,
                hex_color(&config.style.border_soft),
                config.style.border_width,
            );
            self.text.draw(
                pixmap,
                &output.name,
                section.x + 12.0,
                section.y + 9.0,
                11.0,
                hex_color(&config.style.muted),
            );
            let rows_y = section.y + header_height;

            for workspace_index in 0..max_workspace_count {
                let row = Rect {
                    x: section.x + 7.0,
                    y: rows_y + workspace_index as f32 * row_height,
                    width: section.width - 14.0,
                    height: row_height,
                };
                if workspace_index > 0 {
                    fill_rect(
                        pixmap,
                        row.x,
                        row.y,
                        row.width,
                        1.0,
                        hex_color(&config.style.border_soft),
                    );
                }
                let Some(workspace) = output.workspaces.get(workspace_index) else {
                    continue;
                };
                let selected = selection.filter(|selection| {
                    selection.output == output.name && selection.workspace_id == workspace.id
                });
                let hovered = self.workspace_hovered(workspace.id);
                if selected.is_some() || workspace.active || workspace.focused || hovered {
                    fill_rounded_rect(
                        pixmap,
                        row.x + 2.0,
                        row.y + 3.0,
                        row.width - 4.0,
                        row.height - 6.0,
                        config.style.radius,
                        if selected.is_some() || hovered {
                            hex_color(&config.style.surface_hover)
                        } else {
                            hex_color(&config.style.surface)
                        },
                    );
                }
                if selected.is_some() || hovered {
                    stroke_rounded_rect(
                        pixmap,
                        row.x + 2.5,
                        row.y + 3.5,
                        row.width - 5.0,
                        row.height - 7.0,
                        config.style.radius,
                        hex_color(&config.style.selection_border),
                        1.0,
                    );
                }
                zones.push(HitZone {
                    rect: row,
                    target: HitTarget::Workspace {
                        workspace_id: workspace.id,
                        column_count: workspace.columns.len(),
                    },
                });

                let label = workspace.label();
                self.text.draw(
                    pixmap,
                    &label,
                    row.x + 9.0,
                    row.y + (row.height - 11.0) * 0.5,
                    11.0,
                    if selected.is_some() || workspace.active || workspace.focused {
                        hex_color(&config.style.text)
                    } else {
                        hex_color(&config.style.muted)
                    },
                );
                fill_rounded_rect(
                    pixmap,
                    row.x + 1.5,
                    row.y + 10.0,
                    if workspace.active || workspace.focused {
                        3.0
                    } else {
                        1.0
                    },
                    (row.height - 20.0).max(8.0),
                    1.0,
                    if workspace.urgent {
                        hex_color(&config.style.urgent)
                    } else if workspace.active || workspace.focused {
                        hex_color(&config.style.accent)
                    } else {
                        hex_color(&config.style.border_soft)
                    },
                );

                let canvas = Rect {
                    x: row.x + 40.0,
                    y: row.y + 5.0,
                    width: row.width - 47.0,
                    height: row.height - 10.0,
                };
                zones.push(HitZone {
                    rect: canvas,
                    target: HitTarget::Canvas {
                        output: output.name.clone(),
                        workspace_id: workspace.id,
                        rect: canvas,
                    },
                });
                self.draw_workspace_strip_windows(
                    pixmap,
                    output,
                    workspace,
                    canvas,
                    config,
                    selected.and_then(|selection| selection.window_id),
                    zones,
                );
            }
            section_x += section_width + output_gap;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_workspace_strip_windows(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        workspace: &WorkspaceModel,
        canvas: Rect,
        config: &Config,
        selected_window: Option<u64>,
        zones: &mut Vec<HitZone>,
    ) {
        let max_icon = config.style.icon_size.max(28) as f32;
        let column_count = workspace.columns.len().max(1);
        let column_step = ((canvas.width - 8.0) / column_count as f32)
            .min(max_icon + 18.0)
            .max(20.0);
        let mut column_x = canvas.x + 4.0;

        for windows in workspace.columns.values() {
            let row_count = windows.len().max(1);
            let row_gap = 3.0;
            let size = max_icon
                .min(
                    (canvas.height - row_gap * row_count.saturating_sub(1) as f32)
                        / row_count as f32,
                )
                .min(column_step - 5.0)
                .max(14.0);
            let stack_height =
                size * row_count as f32 + row_gap * row_count.saturating_sub(1) as f32;
            let mut window_y = canvas.y + (canvas.height - stack_height) * 0.5;
            for window in windows.values() {
                let rect = Rect {
                    x: column_x + (column_step - size) * 0.5,
                    y: window_y,
                    width: size,
                    height: size,
                };
                self.draw_window_icon(
                    pixmap,
                    rect,
                    window,
                    config,
                    selected_window == Some(window.id),
                );
                zones.push(HitZone {
                    rect,
                    target: HitTarget::Window {
                        output: output.name.clone(),
                        workspace_id: workspace.id,
                        window_id: window.id,
                        column: window.column,
                        floating: false,
                        rect,
                        canvas: Some(canvas),
                    },
                });
                window_y += size + row_gap;
            }
            column_x += column_step;
        }

        for window in &workspace.floating {
            let center_x = window.x.unwrap_or(output.width as f64 * 0.5) + window.tile_width * 0.5;
            let center_y =
                window.y.unwrap_or(output.height as f64 * 0.5) + window.tile_height * 0.5;
            let normalized_x = center_x / output.width.max(1) as f64;
            let normalized_y = center_y / output.height.max(1) as f64;
            let size = max_icon.min(canvas.height * 0.7).max(18.0);
            let rect = Rect {
                x: (canvas.x + normalized_x as f32 * canvas.width - size * 0.5)
                    .clamp(canvas.x, canvas.x + canvas.width - size),
                y: (canvas.y + normalized_y as f32 * canvas.height - size * 0.5)
                    .clamp(canvas.y, canvas.y + canvas.height - size),
                width: size,
                height: size,
            };
            fill_circle(
                pixmap,
                rect.x + rect.width * 0.5,
                rect.y + rect.height * 0.5,
                rect.width * 0.53,
                Color::from_rgba8(20, 24, 29, 210),
            );
            self.draw_window_icon(
                pixmap,
                rect,
                window,
                config,
                selected_window == Some(window.id),
            );
            zones.push(HitZone {
                rect,
                target: HitTarget::Window {
                    output: output.name.clone(),
                    workspace_id: workspace.id,
                    window_id: window.id,
                    column: None,
                    floating: true,
                    rect,
                    canvas: Some(canvas),
                },
            });
        }
    }

    fn render_single_popup(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        config: &Config,
        selection: Option<&SelectionVisual>,
        zones: &mut Vec<HitZone>,
    ) {
        let panel_width = config
            .popup
            .width
            .min(pixmap.width().saturating_sub(24))
            .max(240) as f32;
        let panel_height = config
            .popup
            .height
            .min(pixmap.height().saturating_sub(24))
            .max(160) as f32;
        let (mut panel_x, mut panel_y) = match config.popup.position {
            PopupPosition::Center => (
                (pixmap.width() as f32 - panel_width) * 0.5,
                (pixmap.height() as f32 - panel_height) * 0.5,
            ),
            PopupPosition::Fixed => (config.popup.fixed_x as f32, config.popup.fixed_y as f32),
            PopupPosition::Cursor => self
                .popup_origins
                .get(&output.name)
                .copied()
                .map(|(x, y)| (x - panel_width * 0.5, y + 18.0))
                .unwrap_or(((pixmap.width() as f32 - panel_width) * 0.5, 80.0)),
        };
        panel_x = panel_x.clamp(12.0, (pixmap.width() as f32 - panel_width - 12.0).max(12.0));
        panel_y = panel_y.clamp(
            12.0,
            (pixmap.height() as f32 - panel_height - 12.0).max(12.0),
        );

        fill_rounded_rect(
            pixmap,
            panel_x + 7.0,
            panel_y + 9.0,
            panel_width,
            panel_height,
            config.style.radius + 8.0,
            Color::from_rgba8(0, 0, 0, 90),
        );
        fill_rounded_rect(
            pixmap,
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            config.style.radius + 8.0,
            hex_color(&config.style.background),
        );
        stroke_rounded_rect(
            pixmap,
            panel_x,
            panel_y,
            panel_width,
            panel_height,
            config.style.radius + 8.0,
            hex_color(&config.style.border),
            config.style.border_width,
        );
        zones.push(HitZone {
            rect: Rect {
                x: panel_x,
                y: panel_y,
                width: panel_width,
                height: panel_height,
            },
            target: HitTarget::Panel,
        });

        let padding = 16.0;
        let gap = config.style.gap.max(8.0);
        let count = output.workspaces.len().max(1);
        let card_width = ((panel_width - padding * 2.0 - gap * count.saturating_sub(1) as f32)
            / count as f32)
            .max(76.0);
        let card_height = panel_height - 24.0;
        let rail_y = panel_y + 47.0;
        let rail_start = panel_x + padding + card_width * 0.5;
        let rail_end = rail_start + (count.saturating_sub(1)) as f32 * (card_width + gap);
        fill_rect(
            pixmap,
            rail_start,
            rail_y,
            (rail_end - rail_start).max(0.0),
            1.0,
            hex_color(&config.style.border_soft),
        );

        for (index, workspace) in output.workspaces.iter().enumerate() {
            let selected = selection.filter(|selection| {
                selection.output == output.name && selection.workspace_id == workspace.id
            });
            let rect = Rect {
                x: panel_x + padding + index as f32 * (card_width + gap),
                y: panel_y + 12.0,
                width: card_width,
                height: card_height,
            };
            self.draw_popup_workspace_background(
                pixmap,
                rect,
                workspace,
                config,
                selected.is_some(),
            );
            zones.push(HitZone {
                rect,
                target: HitTarget::Workspace {
                    workspace_id: workspace.id,
                    column_count: workspace.columns.len(),
                },
            });

            let label = workspace.label();
            let label_width = self.text.measure(&label, 13.0);
            self.text.draw(
                pixmap,
                &label,
                rect.x + (rect.width - label_width) * 0.5,
                panel_y + 18.0,
                13.0,
                if selected.is_some() || workspace.active || workspace.focused {
                    hex_color(&config.style.text)
                } else {
                    hex_color(&config.style.muted)
                },
            );
            let node_x = rect.x + rect.width * 0.5;
            if selected.is_some() {
                fill_circle(
                    pixmap,
                    node_x,
                    rail_y + 0.5,
                    9.0,
                    hex_color(&config.style.surface_hover),
                );
            }
            fill_circle(
                pixmap,
                node_x,
                rail_y + 0.5,
                if workspace.active || workspace.focused {
                    5.5
                } else {
                    3.5
                },
                if workspace.urgent {
                    hex_color(&config.style.urgent)
                } else if selected.is_some() || workspace.active || workspace.focused {
                    hex_color(&config.style.accent)
                } else {
                    hex_color(&config.style.muted)
                },
            );
            let canvas = Rect {
                x: rect.x + 8.0,
                y: rail_y + 15.0,
                width: rect.width - 16.0,
                height: (rect.y + rect.height - rail_y - 24.0).max(24.0),
            };
            zones.push(HitZone {
                rect: canvas,
                target: HitTarget::Canvas {
                    output: output.name.clone(),
                    workspace_id: workspace.id,
                    rect: canvas,
                },
            });
            self.draw_workspace_windows(
                pixmap,
                output,
                workspace,
                canvas,
                config,
                selected.and_then(|selection| selection.window_id),
                zones,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_workspace_windows(
        &mut self,
        pixmap: &mut Pixmap,
        output: &OutputModel,
        workspace: &WorkspaceModel,
        canvas: Rect,
        config: &Config,
        selected_window: Option<u64>,
        zones: &mut Vec<HitZone>,
    ) {
        let column_widths = workspace
            .columns
            .values()
            .map(|windows| {
                windows
                    .values()
                    .next()
                    .map(|window| window.tile_width.max(1.0) as f32)
                    .unwrap_or(1.0)
            })
            .collect::<Vec<_>>();
        let total_width = column_widths.iter().sum::<f32>().max(1.0);
        let mut column_x = canvas.x;
        for (column_index, (_column, windows)) in workspace.columns.iter().enumerate() {
            let column_width = canvas.width * column_widths[column_index] / total_width;
            let row_heights = windows
                .values()
                .map(|window| window.tile_height.max(1.0) as f32)
                .collect::<Vec<_>>();
            let total_height = row_heights.iter().sum::<f32>().max(1.0);
            let mut row_y = canvas.y;
            for (row_index, window) in windows.values().enumerate() {
                let row_height = canvas.height * row_heights[row_index] / total_height;
                let max_icon = config.style.icon_size.max(34) as f32;
                let size = max_icon
                    .min(column_width - 5.0)
                    .min(row_height - 5.0)
                    .max(14.0);
                let rect = Rect {
                    x: column_x + (column_width - size) * 0.5,
                    y: row_y + (row_height - size) * 0.5,
                    width: size,
                    height: size,
                };
                self.draw_window_icon(
                    pixmap,
                    rect,
                    window,
                    config,
                    selected_window == Some(window.id),
                );
                zones.push(HitZone {
                    rect,
                    target: HitTarget::Window {
                        output: output.name.clone(),
                        workspace_id: workspace.id,
                        window_id: window.id,
                        column: window.column,
                        floating: false,
                        rect,
                        canvas: Some(canvas),
                    },
                });
                if config.popup.show_titles {
                    let title = truncate_label(&window.title, 12);
                    self.text.draw(
                        pixmap,
                        &title,
                        rect.x,
                        (rect.y + rect.height + 2.0).min(canvas.y + canvas.height - 10.0),
                        9.0,
                        hex_color(&config.style.muted),
                    );
                }
                row_y += row_height;
            }
            column_x += column_width;
        }

        for window in &workspace.floating {
            let center_x = window.x.unwrap_or(output.width as f64 * 0.5) + window.tile_width * 0.5;
            let center_y =
                window.y.unwrap_or(output.height as f64 * 0.5) + window.tile_height * 0.5;
            let normalized_x = center_x / output.width.max(1) as f64;
            let normalized_y = center_y / output.height.max(1) as f64;
            let size = (config.style.icon_size as f32 + 12.0)
                .min(canvas.width * 0.3)
                .max(18.0);
            let rect = Rect {
                x: (canvas.x + normalized_x as f32 * canvas.width - size * 0.5)
                    .clamp(canvas.x, canvas.x + canvas.width - size),
                y: (canvas.y + normalized_y as f32 * canvas.height - size * 0.5)
                    .clamp(canvas.y, canvas.y + canvas.height - size),
                width: size,
                height: size,
            };
            fill_circle(
                pixmap,
                rect.x + rect.width * 0.5,
                rect.y + rect.height * 0.5,
                rect.width * 0.52,
                Color::from_rgba8(20, 24, 29, 210),
            );
            self.draw_window_icon(
                pixmap,
                rect,
                window,
                config,
                selected_window == Some(window.id),
            );
            zones.push(HitZone {
                rect,
                target: HitTarget::Window {
                    output: output.name.clone(),
                    workspace_id: workspace.id,
                    window_id: window.id,
                    column: None,
                    floating: true,
                    rect,
                    canvas: Some(canvas),
                },
            });
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

    fn draw_popup_workspace_background(
        &mut self,
        pixmap: &mut Pixmap,
        rect: Rect,
        workspace: &WorkspaceModel,
        config: &Config,
        selected: bool,
    ) {
        let hovered = self.workspace_hovered(workspace.id);
        fill_rounded_rect(
            pixmap,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            config.style.radius + 2.0,
            if selected || hovered {
                hex_color(&config.style.surface_hover)
            } else if workspace.active || workspace.focused {
                hex_color(&config.style.surface)
            } else {
                Color::TRANSPARENT
            },
        );

        if selected || hovered || workspace.urgent {
            stroke_rounded_rect(
                pixmap,
                rect.x + 0.5,
                rect.y + 0.5,
                rect.width - 1.0,
                rect.height - 1.0,
                config.style.radius + 2.0,
                if workspace.urgent {
                    hex_color(&config.style.urgent)
                } else {
                    hex_color(&config.style.selection_border)
                },
                1.0,
            );
        }

        if workspace.active || workspace.focused || workspace.urgent {
            fill_rounded_rect(
                pixmap,
                rect.x + 12.0,
                rect.y + rect.height - 4.0,
                (rect.width - 24.0).max(10.0),
                2.5,
                1.25,
                if workspace.urgent {
                    hex_color(&config.style.urgent)
                } else {
                    hex_color(&config.style.accent)
                },
            );
        }
    }

    fn draw_window_icon(
        &mut self,
        pixmap: &mut Pixmap,
        rect: Rect,
        window: &WindowModel,
        config: &Config,
        selected: bool,
    ) {
        let hovered = self.window_hovered(window.id);
        if selected {
            fill_rounded_rect(
                pixmap,
                rect.x - 3.0,
                rect.y - 3.0,
                rect.width + 6.0,
                rect.height + 6.0,
                config.style.radius,
                hex_color(&config.style.surface_hover),
            );
            stroke_rounded_rect(
                pixmap,
                rect.x - 2.5,
                rect.y - 2.5,
                rect.width + 5.0,
                rect.height + 5.0,
                config.style.radius,
                hex_color(&config.style.selection_border),
                1.0,
            );
        } else if window.urgent {
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
        } else if hovered {
            fill_rounded_rect(
                pixmap,
                rect.x - 2.0,
                rect.y - 2.0,
                rect.width + 4.0,
                rect.height + 4.0,
                config.style.radius,
                hex_color(&config.style.surface_hover),
            );
            stroke_rounded_rect(
                pixmap,
                rect.x - 1.5,
                rect.y - 1.5,
                rect.width + 3.0,
                rect.height + 3.0,
                config.style.radius,
                hex_color(&config.style.selection_border),
                1.0,
            );
        }
        if let Some((width, height, pixels)) = self.icons.get(&window.app_id) {
            blit_icon(
                pixmap,
                rect.x.round() as i32,
                rect.y.round() as i32,
                rect.width.min(rect.height).round() as u32,
                pixels,
                *width,
                *height,
            );
        } else {
            fill_rounded_rect(
                pixmap,
                rect.x,
                rect.y,
                rect.width,
                rect.height,
                5.0,
                hex_color(&config.style.border),
            );
            let fallback = window
                .app_id
                .chars()
                .find(|character| character.is_alphanumeric())
                .unwrap_or('?')
                .to_uppercase()
                .to_string();
            self.text.draw(
                pixmap,
                &fallback,
                rect.x + rect.width * 0.28,
                rect.y + rect.height * 0.17,
                rect.height * 0.55,
                hex_color(&config.style.text),
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
            9.0,
            Color::from_rgba8(10, 12, 15, 210),
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
        self.draw_window_icon(pixmap, rect, &ghost_window, config, false);
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
