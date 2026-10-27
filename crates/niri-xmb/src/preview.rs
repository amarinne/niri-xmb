//! Offline popup rendering for the config studio.
//!
//! The preview is not a mock: it drives the same [`Renderer`] the daemon uses,
//! with the config the editor is currently showing and the daemon's live
//! snapshot. The returned bitmap and hit zones are therefore pixel-identical
//! to the real panel, including icon lookup, titles, combined-output layout,
//! hover emphasis, and selection emphasis.

use crate::config::Config;
use crate::model::{OutputModel, Snapshot, WindowModel, WorkspaceModel};
use crate::render::{HitTarget, Renderer, SelectionVisual};
use crate::surface::{SurfaceInfo, SurfaceKind};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};

/// Which window the preview parks on the focal column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSelection {
    pub output: String,
    pub workspace_id: u64,
    pub window_id: Option<u64>,
}

/// What sits under a point in a rendered [`PreviewFrame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTarget {
    Panel,
    Canvas { workspace_id: u64 },
    Workspace { workspace_id: u64 },
    Window { workspace_id: u64, window_id: u64 },
}

#[derive(Debug, Clone)]
pub struct PreviewZone {
    /// Panel-local logical pixels, relative to the cropped bitmap.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub target: PreviewTarget,
}

#[derive(Debug, Clone)]
pub struct PreviewFrame {
    pub width: u32,
    pub height: u32,
    /// Premultiplied RGBA, ready for `egui::ColorImage::from_rgba_premultiplied`.
    pub rgba: Vec<u8>,
    pub zones: Vec<PreviewZone>,
}

impl PreviewFrame {
    /// Topmost zone under a point in preview-bitmap coordinates.
    pub fn target_at(&self, x: f32, y: f32) -> Option<&PreviewTarget> {
        self.zones
            .iter()
            .rev()
            .find(|zone| {
                x >= zone.x && y >= zone.y && x < zone.x + zone.width && y < zone.y + zone.height
            })
            .map(|zone| &zone.target)
    }
}

/// Persistent renderer + crop state for one preview surface.
pub struct PopupPreview {
    renderer: Renderer,
    surface_id: usize,
    crop_x: f32,
    crop_y: f32,
    zones: Vec<PreviewZone>,
    hover: Option<PreviewTarget>,
}

impl Default for PopupPreview {
    fn default() -> Self {
        Self::new()
    }
}

impl PopupPreview {
    pub fn new() -> Self {
        Self {
            renderer: Renderer::new(),
            surface_id: usize::MAX,
            crop_x: 0.0,
            crop_y: 0.0,
            zones: Vec::new(),
            hover: None,
        }
    }

    /// Match the live panel's input-active border state.
    pub fn set_input_active(&mut self, active: bool) {
        self.renderer.set_panel_input_active(active);
    }

    /// Point the hover emphasis at a panel-local coordinate, or clear it with
    /// `None`. Returns true when the hovered target changed, so the caller can
    /// skip a redundant render.
    pub fn set_pointer(&mut self, pointer: Option<(f32, f32)>) -> bool {
        let target = pointer.and_then(|(x, y)| self.target_at(x, y).cloned());
        if target == self.hover {
            return false;
        }
        self.hover = target.clone();
        match pointer {
            Some((x, y)) => {
                self.renderer.update_hover(
                    self.surface_id,
                    (x + self.crop_x) as f64,
                    (y + self.crop_y) as f64,
                );
            }
            None => {
                self.renderer.clear_hover(self.surface_id);
            }
        }
        true
    }

    fn target_at(&self, x: f32, y: f32) -> Option<&PreviewTarget> {
        self.zones
            .iter()
            .rev()
            .find(|zone| {
                x >= zone.x && y >= zone.y && x < zone.x + zone.width && y < zone.y + zone.height
            })
            .map(|zone| &zone.target)
    }

    /// Render the popup exactly as the daemon would for the current snapshot
    /// and config, cropped to the panel plus its shadow.
    pub fn render(
        &mut self,
        config: &Config,
        snapshot: &Snapshot,
        selection: Option<&PreviewSelection>,
    ) -> Option<PreviewFrame> {
        let host = self.host_output(snapshot, selection)?;
        let output = snapshot.outputs.get(&host)?;
        let width = u32::try_from(output.width.max(1)).ok()?;
        let height = u32::try_from(output.height.max(1)).ok()?;

        // Keep the workspace selection when its window is gone (closed or
        // moved): the daemon normalizes the same way and still draws the
        // workspace row and its titles. Only a workspace that left the host
        // output drops the whole selection.
        let selection = selection.and_then(|selection| {
            let workspace_ok =
                snapshot
                    .workspace(selection.workspace_id)
                    .is_some_and(|workspace| {
                        output
                            .workspaces
                            .iter()
                            .any(|candidate| candidate.id == workspace.id)
                    });
            if !workspace_ok {
                return None;
            }
            let window_ok = selection.window_id.is_none_or(|id| {
                snapshot
                    .window(id)
                    .is_some_and(|window| window.workspace_id == selection.workspace_id)
            });
            Some(PreviewSelection {
                output: selection.output.clone(),
                workspace_id: selection.workspace_id,
                window_id: selection.window_id.filter(|_| window_ok),
            })
        });
        let visual = selection.map(|selection| SelectionVisual {
            output: selection.output.clone(),
            workspace_id: selection.workspace_id,
            window_id: selection.window_id,
            anchors: HashMap::new(),
        });

        // The preview is a still frame; make every animated quantity land on
        // its target immediately instead of simulating a panel that is still
        // opening.
        let mut still = config.clone();
        still.animation.enabled = false;
        let open_outputs = snapshot.outputs.keys().cloned().collect();
        for _ in 0..3 {
            self.renderer
                .update_animations(snapshot, &still, visual.as_ref(), &open_outputs);
        }

        let surface = SurfaceInfo {
            id: self.surface_id,
            kind: SurfaceKind::Popup,
            output: host,
            global_x: 0.0,
            global_y: 0.0,
            width,
            height,
            configured: true,
            frame_ready: true,
            needs_full_redraw: false,
            scale_factor: 1,
        };
        let pixels = {
            let frame = self
                .renderer
                .render(&surface, snapshot, &still, None, visual.as_ref())?;
            frame.pixels.to_vec()
        };

        let zones = self.renderer.hit_zones(self.surface_id);
        let panel = zones
            .iter()
            .find(|zone| matches!(zone.target, HitTarget::Panel))?
            .rect;
        let padding =
            (still.style.shadow_spread + still.style.shadow_offset.abs() + 4.0).clamp(0.0, 64.0);
        let x0 = (panel.x - padding).floor().clamp(0.0, width as f32);
        let y0 = (panel.y - padding).floor().clamp(0.0, height as f32);
        let x1 = (panel.x + panel.width + padding)
            .ceil()
            .clamp(0.0, width as f32);
        let y1 = (panel.y + panel.height + padding)
            .ceil()
            .clamp(0.0, height as f32);
        let crop_width = (x1 - x0).max(1.0) as u32;
        let crop_height = (y1 - y0).max(1.0) as u32;
        self.crop_x = x0;
        self.crop_y = y0;

        let stride = width as usize * 4;
        let mut rgba = Vec::with_capacity(crop_width as usize * crop_height as usize * 4);
        for row in 0..crop_height {
            let start = (y0 as usize + row as usize) * stride + x0 as usize * 4;
            rgba.extend_from_slice(&pixels[start..start + crop_width as usize * 4]);
        }

        let mut cropped = Vec::new();
        for zone in zones {
            let x0z = zone.rect.x.max(x0);
            let y0z = zone.rect.y.max(y0);
            let x1z = (zone.rect.x + zone.rect.width).min(x1);
            let y1z = (zone.rect.y + zone.rect.height).min(y1);
            if x1z <= x0z || y1z <= y0z {
                continue;
            }
            let target = match zone.target {
                HitTarget::Panel => PreviewTarget::Panel,
                HitTarget::Canvas { workspace_id, .. } => PreviewTarget::Canvas { workspace_id },
                HitTarget::Workspace { workspace_id } => PreviewTarget::Workspace { workspace_id },
                HitTarget::Window {
                    workspace_id,
                    window_id,
                    ..
                } => PreviewTarget::Window {
                    workspace_id,
                    window_id,
                },
            };
            cropped.push(PreviewZone {
                x: x0z - x0,
                y: y0z - y0,
                width: x1z - x0z,
                height: y1z - y0z,
                target,
            });
        }
        self.zones = cropped.clone();

        Some(PreviewFrame {
            width: crop_width,
            height: crop_height,
            rgba,
            zones: cropped,
        })
    }

    fn host_output(
        &self,
        snapshot: &Snapshot,
        selection: Option<&PreviewSelection>,
    ) -> Option<String> {
        selection
            .map(|selection| selection.output.as_str())
            .filter(|output| snapshot.outputs.contains_key(*output))
            .or_else(|| snapshot.focused_output())
            .map(str::to_string)
            .or_else(|| snapshot.outputs.keys().next().cloned())
    }
}

/// Build a snapshot from the daemon's `snapshot` JSON response.
pub fn snapshot_from_json(value: &Value) -> Option<Snapshot> {
    let outputs = value["outputs"].as_array()?;
    let mut snapshot = Snapshot {
        outputs: BTreeMap::new(),
        focused_window: value["focused_window"].as_u64(),
        overview_open: false,
        generation: value["generation"].as_u64().unwrap_or(0),
        content_revision: value["content_revision"].as_u64().unwrap_or(0),
    };
    for output in outputs {
        let name = output["name"].as_str()?.to_string();
        let mut model = OutputModel {
            name: name.clone(),
            x: output["x"].as_i64().unwrap_or(0) as i32,
            y: output["y"].as_i64().unwrap_or(0) as i32,
            width: output["width"].as_i64().unwrap_or(0) as i32,
            height: output["height"].as_i64().unwrap_or(0) as i32,
            workspaces: Vec::new(),
        };
        for workspace in output["workspaces"].as_array().into_iter().flatten() {
            let id = workspace["id"].as_u64()?;
            let mut model_workspace = WorkspaceModel {
                id,
                index: workspace["index"].as_u64().unwrap_or(0) as u8,
                name: workspace["name"].as_str().map(str::to_string),
                active: workspace["active"].as_bool().unwrap_or(false),
                focused: workspace["focused"].as_bool().unwrap_or(false),
                urgent: workspace["urgent"].as_bool().unwrap_or(false),
                active_window: workspace["active_window"].as_u64(),
                columns: BTreeMap::new(),
                floating: Vec::new(),
            };
            for entry in workspace["windows"].as_array().into_iter().flatten() {
                let window = WindowModel {
                    id: entry["id"].as_u64()?,
                    workspace_id: entry["workspace_id"].as_u64().unwrap_or(id),
                    app_id: entry["app_id"].as_str().unwrap_or("").to_string(),
                    title: entry["title"].as_str().unwrap_or("").to_string(),
                    focused: entry["focused"].as_bool().unwrap_or(false),
                    urgent: entry["urgent"].as_bool().unwrap_or(false),
                    floating: entry["floating"].as_bool().unwrap_or(false),
                    column: entry["column"].as_u64().map(|value| value as usize),
                    tile_width: entry["tile_width"].as_f64().unwrap_or(0.0),
                    tile_height: entry["tile_height"].as_f64().unwrap_or(0.0),
                    x: entry["x"].as_f64(),
                    y: entry["y"].as_f64(),
                };
                if window.floating {
                    model_workspace.floating.push(window);
                } else {
                    let column = window.column.unwrap_or(0);
                    let row = entry["row"].as_u64().unwrap_or(0) as usize;
                    model_workspace
                        .columns
                        .entry(column)
                        .or_default()
                        .insert(row, window);
                }
            }
            model.workspaces.push(model_workspace);
        }
        snapshot.outputs.insert(name, model);
    }
    Some(snapshot)
}

impl PreviewSelection {
    /// Live status selection when the daemon has one, otherwise the window
    /// Niri reports as focused.
    pub fn from_live(status: &Value, snapshot: &Snapshot) -> Option<Self> {
        let selection = &status["selection"];
        let output = selection["output"].as_str();
        let workspace_id = selection["workspace_id"].as_u64();
        if let (Some(output), Some(workspace_id)) = (output, workspace_id)
            && snapshot
                .outputs
                .get(output)
                .is_some_and(|model| model.workspaces.iter().any(|ws| ws.id == workspace_id))
        {
            return Some(Self {
                output: output.to_string(),
                workspace_id,
                window_id: selection["window_id"].as_u64(),
            });
        }

        if let Some(window_id) = snapshot.focused_window
            && let Some(window) = snapshot.window(window_id)
            && let Some(output) = snapshot.outputs.values().find(|output| {
                output
                    .workspaces
                    .iter()
                    .any(|ws| ws.id == window.workspace_id)
            })
        {
            return Some(Self {
                output: output.name.clone(),
                workspace_id: window.workspace_id,
                window_id: Some(window_id),
            });
        }

        let output = snapshot
            .outputs
            .values()
            .find(|output| output.workspaces.iter().any(|ws| ws.focused))?;
        let workspace = output
            .workspaces
            .iter()
            .find(|workspace| workspace.focused)
            .or_else(|| output.workspaces.iter().find(|workspace| workspace.active))
            .or_else(|| output.workspaces.first())?;
        Some(Self {
            output: output.name.clone(),
            workspace_id: workspace.id,
            window_id: workspace.active_window,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snapshot() -> Snapshot {
        let mut outputs = BTreeMap::new();
        let window = WindowModel {
            id: 7,
            workspace_id: 1,
            app_id: "kitty".into(),
            title: "Terminal".into(),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(0),
            tile_width: 1280.0,
            tile_height: 720.0,
            x: None,
            y: None,
        };
        let mut columns = BTreeMap::new();
        columns
            .entry(0)
            .or_insert_with(BTreeMap::new)
            .insert(0, window);
        outputs.insert(
            "DP-1".into(),
            OutputModel {
                name: "DP-1".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                workspaces: vec![WorkspaceModel {
                    id: 1,
                    index: 1,
                    name: None,
                    active: true,
                    focused: true,
                    urgent: false,
                    active_window: Some(7),
                    columns,
                    floating: Vec::new(),
                }],
            },
        );
        Snapshot {
            outputs,
            focused_window: Some(7),
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    #[test]
    fn config_json_round_trips_for_the_preview() {
        let config = Config::default();
        let value = serde_json::to_value(&config).unwrap();
        serde_json::from_value::<Config>(value).unwrap();
    }

    #[test]
    fn snapshot_json_keeps_tiling_and_selection() {
        let value = json!({
            "generation": 2,
            "focused_window": 7,
            "outputs": [{
                "name": "DP-1",
                "x": 0,
                "y": 0,
                "width": 2560,
                "height": 1440,
                "workspaces": [{
                    "id": 1,
                    "index": 1,
                    "active": true,
                    "focused": true,
                    "active_window": 7,
                    "windows": [{
                        "id": 7,
                        "workspace_id": 1,
                        "app_id": "kitty",
                        "title": "Terminal",
                        "focused": true,
                        "column": 0,
                        "row": 0
                    }]
                }]
            }]
        });
        let snapshot = snapshot_from_json(&value).unwrap();
        assert_eq!(snapshot.focused_window, Some(7));
        assert_eq!(snapshot.generation, 2);
        let workspace = snapshot.workspace(1).unwrap();
        assert_eq!(workspace.active_window, Some(7));
        assert_eq!(
            workspace.window_slot(7),
            Some(crate::model::WindowSlot { column: 0, row: 0 })
        );
    }

    fn stacked_snapshot() -> Snapshot {
        let first = WindowModel {
            id: 7,
            workspace_id: 1,
            app_id: "kitty".into(),
            title: "Terminal".into(),
            focused: true,
            urgent: false,
            floating: false,
            column: Some(0),
            tile_width: 1280.0,
            tile_height: 720.0,
            x: None,
            y: None,
        };
        let second = WindowModel {
            id: 8,
            workspace_id: 1,
            app_id: "editor".into(),
            title: "Editor".into(),
            focused: false,
            urgent: false,
            floating: false,
            column: Some(0),
            tile_width: 1280.0,
            tile_height: 720.0,
            x: None,
            y: None,
        };
        let mut columns = BTreeMap::new();
        columns
            .entry(0)
            .or_insert_with(BTreeMap::new)
            .insert(0, first);
        columns
            .entry(0)
            .or_insert_with(BTreeMap::new)
            .insert(1, second);
        let mut outputs = BTreeMap::new();
        outputs.insert(
            "DP-1".into(),
            OutputModel {
                name: "DP-1".into(),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                workspaces: vec![WorkspaceModel {
                    id: 1,
                    index: 1,
                    name: None,
                    active: true,
                    focused: true,
                    urgent: false,
                    active_window: Some(7),
                    columns,
                    floating: Vec::new(),
                }],
            },
        );
        Snapshot {
            outputs,
            focused_window: Some(7),
            overview_open: false,
            generation: 1,
            content_revision: 0,
        }
    }

    #[test]
    fn preview_keeps_workspace_selection_when_window_closed() {
        // The daemon normalizes a closed selected window back to its
        // workspace instead of hiding the panel; the preview must do the
        // same or its titles vanish while the real popup keeps them.
        let snapshot = snapshot();
        let config = Config::default();
        let selection = PreviewSelection {
            output: "DP-1".into(),
            workspace_id: 1,
            window_id: Some(999),
        };
        let mut preview = PopupPreview::new();
        let frame = preview
            .render(&config, &snapshot, Some(&selection))
            .expect("workspace selection must still render");
        assert!(frame.width > 0 && frame.height > 0);
        assert!(
            frame.zones.iter().any(|zone| matches!(
                zone.target,
                PreviewTarget::Window { .. }
                    | PreviewTarget::Workspace { .. }
                    | PreviewTarget::Canvas { .. }
            )),
            "closed window must fall back to the workspace, not an empty frame"
        );
    }

    #[test]
    fn preview_title_toggle_changes_shared_renderer_output() {
        // A stacked column draws a title beside every row through the
        // shared daemon renderer, so flipping `show_titles` must change the
        // preview bitmap at identical dimensions.
        let snapshot = stacked_snapshot();
        let selection = PreviewSelection {
            output: "DP-1".into(),
            workspace_id: 1,
            window_id: Some(7),
        };
        let mut titled = Config::default();
        titled.popup.show_titles = true;
        let mut plain = Config::default();
        plain.popup.show_titles = false;
        let mut preview = PopupPreview::new();
        let with_titles = preview
            .render(&titled, &snapshot, Some(&selection))
            .expect("titled preview renders");
        let mut preview = PopupPreview::new();
        let without_titles = preview
            .render(&plain, &snapshot, Some(&selection))
            .expect("untitled preview renders");
        assert_eq!(with_titles.width, without_titles.width);
        assert_eq!(with_titles.height, without_titles.height);
        assert_ne!(
            with_titles.rgba, without_titles.rgba,
            "show_titles must change the preview bitmap through the shared renderer"
        );
    }

    #[test]
    fn preview_matches_daemon_renderer_for_same_inputs() {
        // The preview is an exact duplicate instance: drive a bare daemon
        // `Renderer` with the same still config, selection, and surface, and
        // the cropped bytes must equal the preview frame.
        use crate::render::Renderer;
        use crate::surface::{SurfaceInfo, SurfaceKind};
        use std::collections::HashSet;

        let snapshot = snapshot();
        let mut config = Config::default();
        config.popup.show_titles = true;
        let selection = PreviewSelection::from_live(&json!({}), &snapshot).unwrap();
        let mut preview = PopupPreview::new();
        let frame = preview
            .render(&config, &snapshot, Some(&selection))
            .expect("preview renders");

        let visual = SelectionVisual {
            output: selection.output.clone(),
            workspace_id: selection.workspace_id,
            window_id: selection.window_id,
            anchors: HashMap::new(),
        };
        let mut still = config.clone();
        still.animation.enabled = false;
        let open_outputs: HashSet<String> = snapshot.outputs.keys().cloned().collect();
        let mut renderer = Renderer::new();
        for _ in 0..3 {
            renderer.update_animations(&snapshot, &still, Some(&visual), &open_outputs);
        }
        let output = snapshot.outputs.get("DP-1").unwrap();
        let (width, height) = (output.width.max(1) as u32, output.height.max(1) as u32);
        let surface = SurfaceInfo {
            id: 7,
            kind: SurfaceKind::Popup,
            output: "DP-1".into(),
            global_x: 0.0,
            global_y: 0.0,
            width,
            height,
            configured: true,
            frame_ready: true,
            needs_full_redraw: false,
            scale_factor: 1,
        };
        let rendered = renderer
            .render(&surface, &snapshot, &still, None, Some(&visual))
            .expect("reference renderer renders");
        let pixels = rendered.pixels.to_vec();
        let zones = renderer.hit_zones(7).to_vec();
        let panel = zones
            .iter()
            .find(|zone| matches!(zone.target, crate::render::HitTarget::Panel))
            .expect("reference panel zone")
            .rect;
        let padding =
            (still.style.shadow_spread + still.style.shadow_offset.abs() + 4.0).clamp(0.0, 64.0);
        let x0 = (panel.x - padding).floor().clamp(0.0, width as f32);
        let y0 = (panel.y - padding).floor().clamp(0.0, height as f32);
        let x1 = (panel.x + panel.width + padding)
            .ceil()
            .clamp(0.0, width as f32);
        let y1 = (panel.y + panel.height + padding)
            .ceil()
            .clamp(0.0, height as f32);
        let crop_width = (x1 - x0).max(1.0) as u32;
        let crop_height = (y1 - y0).max(1.0) as u32;
        assert_eq!(frame.width, crop_width);
        assert_eq!(frame.height, crop_height);
        let stride = width as usize * 4;
        let mut expected = Vec::with_capacity(crop_width as usize * crop_height as usize * 4);
        for row in 0..crop_height {
            let start = (y0 as usize + row as usize) * stride + x0 as usize * 4;
            expected.extend_from_slice(&pixels[start..start + crop_width as usize * 4]);
        }
        assert_eq!(
            frame.rgba, expected,
            "preview frame must equal the daemon renderer output"
        );
    }

    #[test]
    fn rendered_frame_has_real_hit_zones() {
        let snapshot = snapshot();
        let config = Config::default();
        let selection = PreviewSelection::from_live(&json!({}), &snapshot).unwrap();
        let mut preview = PopupPreview::new();
        let frame = preview
            .render(&config, &snapshot, Some(&selection))
            .unwrap();
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.rgba.len(),
            frame.width as usize * frame.height as usize * 4
        );
        let window = frame
            .zones
            .iter()
            .find(|zone| matches!(zone.target, PreviewTarget::Window { window_id: 7, .. }))
            .expect("window zone");
        assert!(matches!(
            frame.target_at(
                window.x + window.width * 0.5,
                window.y + window.height * 0.5
            ),
            Some(PreviewTarget::Window { window_id: 7, .. })
        ));
    }
}
