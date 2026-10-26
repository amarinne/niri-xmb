//! Native editor for niri-xmb TOML.
//!
//! The editor loads the real schema through `niri-xmb config-json`, writes only
//! changed paths through `toml_edit`, validates with `config-check`, and saves
//! atomically. It does not talk to the daemon except through the existing
//! `restart` command after an explicit Save & Restart.

use anyhow::{Context, Result};
use eframe::egui;
use egui::{
    Align2, Color32, ComboBox, FontId, Pos2, Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind,
    TextureHandle, TextureOptions, Vec2,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_CONFIG: &str = "~/.config/niri-xmb/config.toml";
const CONTROLLER_IMAGE: &[u8] = include_bytes!("../../../assets/controller-render.png");
const ACTIONS: &[&str] = &[
    "pointer.left",
    "pointer.right",
    "pointer.middle",
    "pointer.scroll-up",
    "pointer.scroll-down",
    "xmb.open",
    "niri.focus-workspace-prev",
    "niri.focus-workspace-next",
    "niri.focus-window-prev",
    "niri.focus-window-next",
    "niri.move-window-workspace-prev",
    "niri.move-window-workspace-next",
    "niri.move-column-left",
    "niri.move-column-right",
    "niri.focus-monitor-prev",
    "niri.focus-monitor-next",
    "niri.move-window-monitor-prev",
    "niri.move-window-monitor-next",
    "niri.toggle-overview",
    "niri.close-window",
];
const XMB_ACTIONS: &[&str] = &[
    "up", "down", "left", "right", "tab", "back", "confirm", "close", "open",
];

struct Hotspot {
    control: &'static str,
    code: Option<&'static str>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    circle: bool,
}

const HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "LT",
        code: None,
        x: 0.250,
        y: 0.154,
        w: 0.105,
        h: 0.036,
        circle: false,
    },
    Hotspot {
        control: "RT",
        code: None,
        x: 0.825,
        y: 0.154,
        w: 0.105,
        h: 0.036,
        circle: false,
    },
    Hotspot {
        control: "LB",
        code: Some("BTN_TL"),
        x: 0.250,
        y: 0.234,
        w: 0.125,
        h: 0.050,
        circle: false,
    },
    Hotspot {
        control: "RB",
        code: Some("BTN_TR"),
        x: 0.775,
        y: 0.234,
        w: 0.125,
        h: 0.050,
        circle: false,
    },
    Hotspot {
        control: "L STICK",
        code: None,
        x: 0.220,
        y: 0.468,
        w: 0.102,
        h: 0.102,
        circle: true,
    },
    Hotspot {
        control: "D-PAD",
        code: None,
        x: 0.352,
        y: 0.638,
        w: 0.124,
        h: 0.124,
        circle: true,
    },
    Hotspot {
        control: "R STICK",
        code: None,
        x: 0.618,
        y: 0.668,
        w: 0.102,
        h: 0.102,
        circle: true,
    },
    Hotspot {
        control: "Y",
        code: Some("BTN_NORTH"),
        x: 0.778,
        y: 0.409,
        w: 0.071,
        h: 0.071,
        circle: true,
    },
    Hotspot {
        control: "X",
        code: Some("BTN_WEST"),
        x: 0.701,
        y: 0.500,
        w: 0.071,
        h: 0.071,
        circle: true,
    },
    Hotspot {
        control: "B",
        code: Some("BTN_EAST"),
        x: 0.848,
        y: 0.490,
        w: 0.071,
        h: 0.071,
        circle: true,
    },
    Hotspot {
        control: "A",
        code: Some("BTN_SOUTH"),
        x: 0.771,
        y: 0.585,
        w: 0.071,
        h: 0.071,
        circle: true,
    },
    Hotspot {
        control: "SELECT",
        code: Some("BTN_SELECT"),
        x: 0.395,
        y: 0.470,
        w: 0.058,
        h: 0.050,
        circle: false,
    },
    Hotspot {
        control: "START",
        code: Some("BTN_START"),
        x: 0.585,
        y: 0.470,
        w: 0.058,
        h: 0.050,
        circle: false,
    },
    Hotspot {
        control: "GUIDE",
        code: Some("BTN_MODE"),
        x: 0.500,
        y: 0.452,
        w: 0.070,
        h: 0.070,
        circle: true,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Controller,
    Xmb,
    Style,
    Bars,
    Preview,
    Save,
}

#[derive(Clone, Copy)]
enum Confirm {
    Save,
    Restart,
}

struct StudioApp {
    niri_xmb: PathBuf,
    config_path: String,
    model: Value,
    loaded: Value,
    sha256: String,
    tab: Tab,
    selected: &'static str,
    context: String,
    status: String,
    error: String,
    candidate: String,
    confirm: Option<Confirm>,
    texture: Option<TextureHandle>,
}

impl StudioApp {
    fn new(cc: &eframe::CreationContext<'_>, config: PathBuf, niri_xmb: PathBuf) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self {
            niri_xmb,
            config_path: config.display().to_string(),
            model: Value::Null,
            loaded: Value::Null,
            sha256: String::new(),
            tab: Tab::Controller,
            selected: "A",
            context: "base".to_string(),
            status: String::new(),
            error: String::new(),
            candidate: String::new(),
            confirm: None,
            texture: None,
        };
        app.load();
        app
    }

    fn ensure_texture(&mut self, ctx: &egui::Context) {
        if self.texture.is_some() {
            return;
        }
        let Ok(image) = image::load_from_memory(CONTROLLER_IMAGE) else {
            return;
        };
        let rgba = image.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
        self.texture = Some(ctx.load_texture("controller-render", pixels, TextureOptions::LINEAR));
    }

    fn load(&mut self) {
        let result = load_config(&self.niri_xmb, &json!({"path": self.config_path}));
        if result["ok"] != true {
            self.model = Value::Null;
            self.loaded = Value::Null;
            self.error = result["error"]
                .as_str()
                .unwrap_or("load failed")
                .to_string();
            self.status = String::new();
            return;
        }
        self.model = result["config"].clone();
        self.loaded = self.model.clone();
        self.sha256 = result["sha256"].as_str().unwrap_or("").to_string();
        self.config_path = result["path"]
            .as_str()
            .unwrap_or(&self.config_path)
            .to_string();
        self.error.clear();
        self.candidate.clear();
        self.status = "Loaded".to_string();
    }

    fn dirty(&self) -> bool {
        self.model != Value::Null && self.model != self.loaded
    }

    fn patch(&self) -> Patch {
        compute_patch(&self.loaded, &self.model)
    }

    fn validate(&mut self) {
        if self.model == Value::Null {
            return;
        }
        let patch = self.patch();
        if patch.is_empty() {
            self.error.clear();
            self.candidate.clear();
            self.status = "No changes".to_string();
            return;
        }
        let result = validate_patch(
            &self.niri_xmb,
            &json!({
                "path": self.config_path,
                "sha256": self.sha256,
                "patch": patch.to_json()
            }),
            false,
        );
        if result["ok"] == true {
            self.error.clear();
            self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
            self.status = "Valid".to_string();
        } else {
            self.error = result["error"]
                .as_str()
                .unwrap_or("validation failed")
                .to_string();
            self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
            self.status = "Invalid".to_string();
        }
    }

    fn save(&mut self, restart_after: bool) {
        if self.model == Value::Null {
            return;
        }
        let patch = self.patch();
        if patch.is_empty() {
            self.status = "No changes".to_string();
            return;
        }
        let result = validate_patch(
            &self.niri_xmb,
            &json!({
                "path": self.config_path,
                "sha256": self.sha256,
                "patch": patch.to_json()
            }),
            true,
        );
        if result["ok"] != true {
            self.error = result["error"]
                .as_str()
                .unwrap_or("save failed")
                .to_string();
            self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
            self.status = "Not saved".to_string();
            return;
        }
        self.loaded = self.model.clone();
        self.sha256 = result["sha256"].as_str().unwrap_or("").to_string();
        self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
        self.error.clear();
        self.status = "Saved".to_string();
        if restart_after {
            let result = restart(&self.niri_xmb, &json!({"path": self.config_path}));
            if result["ok"] == true {
                self.status = "Saved and restarted".to_string();
            } else {
                self.error = result["error"]
                    .as_str()
                    .unwrap_or("restart failed")
                    .to_string();
                self.status = "Saved; restart failed".to_string();
            }
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.strong("niri-xmb config");
                ui.add(egui::TextEdit::singleline(&mut self.config_path).desired_width(420.0));
                if ui.button("Load").clicked() {
                    self.load();
                }
                if ui.button("Validate").clicked() {
                    self.validate();
                }
                if ui.button("Save").clicked() && self.dirty() {
                    self.confirm = Some(Confirm::Save);
                }
                if ui.button("Save & Restart").clicked() && self.dirty() {
                    self.confirm = Some(Confirm::Restart);
                }
                if ui.button("Revert").clicked() {
                    self.model = self.loaded.clone();
                    self.error.clear();
                    self.candidate.clear();
                    self.status = "Reverted".to_string();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.dirty() {
                        ui.colored_label(egui::Color32::from_rgb(230, 170, 70), "edited");
                    } else if !self.status.is_empty() {
                        ui.label(&self.status);
                    }
                });
            });
            if !self.error.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(230, 100, 100), &self.error);
            }
            ui.add_space(3.0);
        });
    }

    fn controller_panel(&mut self, ui: &mut egui::Ui) {
        ui.heading("Controller");
        ui.add_space(4.0);
        let width = ui.available_width().min(600.0);
        let height = width * 861.0 / 1280.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        if let Some(texture) = &self.texture {
            ui.painter().image(
                texture.id(),
                rect,
                Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        for hotspot in HOTSPOTS {
            let center = Pos2::new(
                rect.min.x + hotspot.x * rect.width(),
                rect.min.y + hotspot.y * rect.height(),
            );
            let hit = Rect::from_center_size(
                center,
                Vec2::new(hotspot.w * rect.width(), hotspot.h * rect.height()),
            );
            let selected = self.selected == hotspot.control;
            let response = ui.interact(hit, ui.id().with(hotspot.control), Sense::click());
            let stroke = if selected {
                Stroke::new(2.0_f32, Color32::from_rgb(116, 162, 255))
            } else if response.hovered() {
                Stroke::new(1.5_f32, Color32::from_rgb(220, 230, 245))
            } else {
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(116, 162, 255, 110))
            };
            if selected {
                ui.painter().rect_filled(
                    hit,
                    7.0,
                    Color32::from_rgba_unmultiplied(116, 162, 255, 30),
                );
            }
            if hotspot.circle {
                ui.painter()
                    .circle_stroke(center, hit.width().min(hit.height()) / 2.0, stroke);
            } else {
                ui.painter()
                    .rect_stroke(hit, 6.0, stroke, StrokeKind::Inside);
            }
            ui.painter().text(
                Pos2::new(center.x, hit.max.y + 3.0),
                Align2::CENTER_TOP,
                hotspot.control,
                FontId::monospace(8.0),
                Color32::from_gray(160),
            );
            if response.clicked() {
                self.selected = hotspot.control;
            }
        }
        ui.add_space(6.0);
        ui.separator();
        self.inspector(ui);
    }

    fn inspector(&mut self, ui: &mut egui::Ui) {
        let code = HOTSPOTS
            .iter()
            .find(|hotspot| hotspot.control == self.selected)
            .and_then(|hotspot| hotspot.code);
        ui.horizontal(|ui| {
            ui.strong(self.selected);
            if let Some(code) = code {
                ui.monospace(code);
            }
        });
        let Some(code) = code else {
            let hint = match self.selected {
                "L STICK" | "R STICK" => "Pointer/scroll settings are in Controller.",
                "LT" | "RT" => "Trigger acceleration is in Controller.",
                "D-PAD" => "D-pad actions resolve through the active layer or XMB.",
                _ => "",
            };
            if !hint.is_empty() {
                ui.label(hint);
            }
            return;
        };
        let mut contexts = vec!["base".to_string(), "xmb".to_string()];
        if let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object() {
            contexts.extend(layers.keys().cloned());
        }
        if !contexts.contains(&self.context) {
            self.context = "base".to_string();
        }
        ui.horizontal(|ui| {
            ui.label("Context");
            ComboBox::from_id_salt("binding-context")
                .selected_text(&self.context)
                .show_ui(ui, |ui| {
                    for context in &contexts {
                        ui.selectable_value(&mut self.context, context.clone(), context);
                    }
                });
        });
        let map_path = binding_path(&self.context);
        let current = get_path(&self.model, &map_path)
            .and_then(Value::as_object)
            .and_then(|map| map.get(code))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut next = current.clone();
        ui.horizontal(|ui| {
            ui.label("Action");
            ComboBox::from_id_salt("binding-action")
                .selected_text(if next.is_empty() { "unbound" } else { &next })
                .show_ui(ui, |ui| {
                    for action in ACTIONS {
                        ui.selectable_value(&mut next, (*action).to_string(), *action);
                    }
                });
        });
        if next != current {
            if next.is_empty() {
                remove_map_key(&mut self.model, &map_path, code);
            } else {
                set_map_value(&mut self.model, &map_path, code, &next);
            }
        }
        let map = get_path(&self.model, &map_path)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if map.is_empty() {
            ui.label("No bindings in this context.");
        } else {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for (code, action) in map {
                    ui.monospace(format!("{code} → {}", action.as_str().unwrap_or("")));
                }
            });
        }
    }

    fn controller_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Device");
        self.bool_field(ui, "gamepad.enabled", "Enabled");
        self.combo_field(
            ui,
            "gamepad.preset",
            "Preset",
            &["none", "auto", "xbox", "ds4"],
        );
        self.list_field(ui, "gamepad.devices", "Device paths");
        self.list_field(ui, "gamepad.open_combo", "Open combo");
        self.combo_field(
            ui,
            "gamepad.mode.default",
            "Default mode",
            &["game", "navi"],
        );
        self.list_field(ui, "gamepad.mode.toggle", "Mode toggle");
        self.bool_field(ui, "gamepad.mode.grab_in_navi", "Grab in navi");
        self.bool_field(
            ui,
            "gamepad.mode.reset_on_disconnect",
            "Reset on disconnect",
        );
        ui.add_space(10.0);
        ui.heading("Pointer");
        self.bool_field(ui, "gamepad.navi.pointer.enabled", "Enabled");
        self.combo_field(
            ui,
            "gamepad.navi.pointer.stick",
            "Stick",
            &["left", "right"],
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.deadzone",
            "Deadzone",
            0.0,
            1.0,
            0.005,
        );
        self.f32_field(ui, "gamepad.navi.pointer.curve", "Curve", 0.05, 10.0, 0.05);
        self.f32_field(
            ui,
            "gamepad.navi.pointer.speed",
            "Speed",
            0.0,
            20000.0,
            10.0,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.max_step",
            "Max step",
            0.0,
            1000.0,
            1.0,
        );
        self.bool_field(ui, "gamepad.navi.pointer.invert_y", "Invert Y");
        self.combo_field(
            ui,
            "gamepad.navi.pointer.scroll_stick",
            "Scroll stick",
            &["left", "right"],
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.scroll_deadzone",
            "Scroll deadzone",
            0.0,
            1.0,
            0.005,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.scroll_speed",
            "Scroll speed",
            0.0,
            1000.0,
            0.1,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.trigger_threshold",
            "Layer threshold",
            0.0,
            1.0,
            0.01,
        );
        ui.add_space(10.0);
        ui.heading("Trigger acceleration");
        self.f32_field(
            ui,
            "gamepad.navi.pointer.trigger_deadzone",
            "Deadzone",
            0.0,
            0.9,
            0.005,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.trigger_curve",
            "Curve",
            0.05,
            5.0,
            0.05,
        );
        ui.label("Pointer");
        self.bool_field(ui, "gamepad.navi.pointer.accel.enabled", "Enabled");
        self.f32_field(
            ui,
            "gamepad.navi.pointer.accel.decel_floor",
            "LT floor",
            0.01,
            1.0,
            0.01,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.accel.boost_cap",
            "RT cap",
            1.0,
            10.0,
            0.05,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.accel.smoothing_ms",
            "Smoothing ms",
            0.0,
            500.0,
            1.0,
        );
        ui.label("Scroll");
        self.bool_field(ui, "gamepad.navi.pointer.scroll_accel.enabled", "Enabled");
        self.f32_field(
            ui,
            "gamepad.navi.pointer.scroll_accel.decel_floor",
            "LT floor",
            0.01,
            1.0,
            0.01,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.scroll_accel.boost_cap",
            "RT cap",
            1.0,
            10.0,
            0.05,
        );
        self.f32_field(
            ui,
            "gamepad.navi.pointer.scroll_accel.smoothing_ms",
            "Smoothing ms",
            0.0,
            500.0,
            1.0,
        );
        ui.add_space(10.0);
        self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons");
        self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base bindings");
        self.layers_ui(ui);
    }

    fn xmb_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Panel");
        self.combo_field(
            ui,
            "popup.outputs",
            "Outputs",
            &["focused", "all", "combined"],
        );
        self.combo_field(
            ui,
            "popup.surface_mode",
            "Surface",
            &["fixed", "follow_active", "mirror"],
        );
        self.combo_field(
            ui,
            "popup.position",
            "Position",
            &["cursor", "fixed", "center", "anchored"],
        );
        self.length_field(ui, "popup.width", "Width");
        self.length_field(ui, "popup.height", "Height");
        self.length_field(ui, "popup.fixed_x", "Fixed X");
        self.length_field(ui, "popup.fixed_y", "Fixed Y");
        self.length_field(ui, "popup.canvas_width", "Canvas width");
        self.length_field(ui, "popup.canvas_height", "Canvas height");
        self.u64_field(ui, "popup.follow_delay_ms", "Follow delay ms", 0, 60_000);
        self.bool_field(ui, "popup.show_labels", "Labels");
        self.bool_field(ui, "popup.show_titles", "Titles");
        self.placements_ui(ui);
        ui.add_space(10.0);
        ui.heading("Behavior");
        self.combo_field(
            ui,
            "behavior.navigation",
            "Navigation",
            &["deferred", "live"],
        );
        self.bool_field(ui, "behavior.move_popup", "Border drag");
        self.f32_field(
            ui,
            "behavior.popup_grab_region",
            "Grab region",
            0.0,
            2048.0,
            1.0,
        );
        self.f32_field(
            ui,
            "behavior.drag_threshold",
            "Drag threshold",
            0.0,
            2048.0,
            0.5,
        );
        self.bool_field(ui, "behavior.close_on_outside_click", "Close outside");
        self.bool_field(ui, "behavior.replay_outside_click", "Replay outside");
        self.bool_field(ui, "behavior.close_on_window_click", "Close on window");
        self.bool_field(
            ui,
            "behavior.close_on_workspace_click",
            "Close on workspace",
        );
        self.bool_field(ui, "behavior.close_on_drop", "Close on drop");
        self.bool_field(
            ui,
            "behavior.close_on_external_focus_change",
            "Close external focus",
        );
        self.bool_field(ui, "behavior.close_on_escape", "Close on Escape");
        self.bool_field(ui, "behavior.empty_drop_floats", "Empty drop floats");
        self.bool_field(ui, "behavior.floating_drop_tiles", "Floating drop tiles");
        self.bool_field(ui, "behavior.focus_after_drop", "Focus after drop");
        self.bool_field(
            ui,
            "behavior.snap_cursor_after_drop",
            "Snap cursor after drop",
        );
        ui.add_space(10.0);
        ui.heading("Peek");
        self.bool_field(ui, "peek.enabled", "Enabled");
        self.u64_field(ui, "peek.duration_ms", "Duration ms", 0, 60_000);
        self.bool_field(ui, "peek.pointer_promote", "Pointer promote");
        self.bool_field(ui, "peek.move_popup", "Border move");
        self.u64_field(ui, "peek.arm_grace_ms", "Arm grace ms", 0, 60_000);
        self.u64_field(ui, "peek.demote_ms", "Demote ms", 0, 60_000);
        ui.add_space(10.0);
        ui.heading("Animation");
        self.bool_field(ui, "animation.enabled", "Enabled");
        self.f32_field(ui, "animation.panel_ms", "Panel ms", 0.0, 60_000.0, 1.0);
        self.f32_field(
            ui,
            "animation.emphasis_ms",
            "Emphasis ms",
            0.0,
            60_000.0,
            1.0,
        );
        self.f32_field(ui, "animation.scroll_ms", "Scroll ms", 0.0, 60_000.0, 1.0);
        self.f32_field(ui, "animation.open_scale", "Open scale", 0.0, 1.0, 0.01);
    }

    fn style_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Theme");
        self.text_field(ui, "theme", "Theme file");
        ui.add_space(8.0);
        ui.heading("Colors");
        for (path, label) in [
            ("style.background", "Background"),
            ("style.background_end", "Background end"),
            ("style.surface", "Surface"),
            ("style.surface_hover", "Surface hover"),
            ("style.border", "Border"),
            ("style.border_soft", "Soft border"),
            ("style.accent", "Accent"),
            ("style.selection_border", "Selection"),
            ("style.text", "Text"),
            ("style.muted", "Muted"),
            ("style.urgent", "Urgent"),
            ("style.shadow", "Shadow"),
            ("style.floating_backing", "Floating backing"),
        ] {
            self.color_field(ui, path, label);
        }
        self.combo_field(
            ui,
            "style.urgent_indicator",
            "Urgent indicator",
            &["none", "ring", "glow", "badge"],
        );
        ui.add_space(8.0);
        ui.heading("Geometry");
        for (path, label, min, max, step) in [
            ("style.panel_radius", "Panel radius", 0.0, 2048.0, 0.5),
            ("style.radius", "Small radius", 0.0, 2048.0, 0.5),
            ("style.border_width", "Border width", 0.0, 2048.0, 0.1),
            ("style.dim_alpha", "Dim alpha", 0.0, 1.0, 0.01),
            ("style.panel_padding", "Panel padding", 0.0, 2048.0, 0.5),
            ("style.gap", "Gap", 0.0, 2048.0, 0.5),
            ("style.canvas_gap", "Canvas gap", 0.0, 2048.0, 0.5),
            ("style.column_gap", "Column gap", 0.0, 2048.0, 0.5),
            ("style.stack_gap", "Stack gap", 0.0, 2048.0, 0.5),
            ("style.row_gap", "Row gap", 0.0, 2048.0, 0.5),
            ("style.shadow_offset", "Shadow offset", -2048.0, 2048.0, 0.5),
            ("style.shadow_spread", "Shadow spread", 0.0, 2048.0, 0.5),
            ("style.empty_dot_radius", "Empty dot", 0.0, 2048.0, 0.5),
            ("style.label_font_size", "Label size", 1.0, 512.0, 0.5),
            ("style.title_font_size", "Title size", 1.0, 512.0, 0.5),
            ("style.urgent_ring_width", "Urgent ring", 0.0, 2048.0, 0.1),
            (
                "style.urgent_ring_padding",
                "Urgent padding",
                0.0,
                2048.0,
                0.1,
            ),
            (
                "style.urgent_glow_spread",
                "Urgent spread",
                0.0,
                2048.0,
                0.1,
            ),
            ("style.urgent_glow_alpha", "Urgent alpha", 0.0, 1.0, 0.01),
            (
                "style.urgent_badge_radius",
                "Urgent badge",
                0.0,
                2048.0,
                0.1,
            ),
        ] {
            self.f32_field(ui, path, label, min, max, step);
        }
        self.u64_field(ui, "style.icon_size", "Icon size", 1, 1024);
        self.u64_field(ui, "style.icon_size_focused", "Focused icon size", 1, 1024);
    }

    fn bars_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Bars");
        self.bool_field(ui, "bars_enabled", "Enabled");
        let count = self.model["bars"].as_array().map_or(0, Vec::len);
        let mut remove = None;
        for index in 0..count {
            let label = self.model["bars"][index]["output"]
                .as_str()
                .unwrap_or("")
                .to_string();
            egui::CollapsingHeader::new(format!("Bar {} {label}", index + 1))
                .id_salt(("bar", index))
                .show(ui, |ui| {
                    let base = format!("bars.{index}");
                    self.text_field(ui, &format!("{base}.output"), "Output");
                    self.combo_field(
                        ui,
                        &format!("{base}.edge"),
                        "Edge",
                        &["top", "bottom", "left", "right"],
                    );
                    self.combo_field(
                        ui,
                        &format!("{base}.align"),
                        "Align",
                        &["start", "center", "end"],
                    );
                    self.u64_field(ui, &format!("{base}.width"), "Width", 1, 100_000);
                    self.u64_field(ui, &format!("{base}.height"), "Height", 1, 100_000);
                    self.i64_field(ui, &format!("{base}.margin"), "Margin");
                    self.i64_field(ui, &format!("{base}.offset"), "Offset");
                    self.bool_field(ui, &format!("{base}.allow_column_join"), "Column join");
                    self.bool_field(
                        ui,
                        &format!("{base}.show_workspace_labels"),
                        "Workspace labels",
                    );
                    self.bool_field(ui, &format!("{base}.hide_empty_workspaces"), "Hide empty");
                    self.bool_field(ui, &format!("{base}.show_capsule"), "Capsule");
                    self.bool_field(
                        ui,
                        &format!("{base}.show_active_indicator"),
                        "Active indicator",
                    );
                    self.bool_field(ui, &format!("{base}.scroll_to_switch"), "Wheel switches");
                    self.bool_field(ui, &format!("{base}.scroll_wrap"), "Wheel wraps");
                    self.bool_field(ui, &format!("{base}.scroll_reverse"), "Reverse wheel");
                    self.u64_field(
                        ui,
                        &format!("{base}.scroll_repeat_ms"),
                        "Wheel repeat ms",
                        0,
                        60_000,
                    );
                    if ui.button("Delete bar").clicked() {
                        remove = Some(index);
                    }
                });
        }
        if let Some(index) = remove
            && let Some(bars) = self.model["bars"].as_array_mut()
        {
            bars.remove(index);
        }
        if ui.button("Add bar").clicked()
            && let Some(bars) = self.model["bars"].as_array_mut()
        {
            bars.push(default_bar());
        }
    }

    fn preview_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Preview");
        ui.label("Model preview; the renderer remains authoritative.");
        ui.add_space(8.0);
        self.xmb_preview(ui);
        ui.add_space(12.0);
        self.bar_preview(ui);
    }

    fn save_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("Save");
        ui.label(format!("Path: {}", self.config_path));
        ui.label(if self.dirty() {
            "Unsaved changes"
        } else {
            "Clean"
        });
        ui.horizontal(|ui| {
            if ui.button("Validate").clicked() {
                self.validate();
            }
            if ui.button("Save").clicked() && self.dirty() {
                self.confirm = Some(Confirm::Save);
            }
            if ui.button("Save & Restart").clicked() && self.dirty() {
                self.confirm = Some(Confirm::Restart);
            }
            if ui.button("Revert").clicked() {
                self.model = self.loaded.clone();
                self.error.clear();
                self.candidate.clear();
                self.status = "Reverted".to_string();
            }
        });
        if !self.candidate.is_empty() {
            ui.add_space(6.0);
            ui.label("Validated TOML");
            let mut candidate = self.candidate.clone();
            ui.add(
                egui::TextEdit::multiline(&mut candidate)
                    .code_editor()
                    .desired_rows(20)
                    .interactive(false),
            );
        }
    }

    fn text_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut value).desired_width(320.0))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, Value::String(value));
        }
    }

    fn bool_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let changed = row(ui, label, |ui| ui.checkbox(&mut value, "").changed());
        if changed {
            set_json_path(&mut self.model, path, Value::Bool(value));
        }
    }

    fn f32_field(
        &mut self,
        ui: &mut egui::Ui,
        path: &str,
        label: &str,
        min: f64,
        max: f64,
        speed: f64,
    ) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let changed = row(ui, label, |ui| {
            ui.add(
                egui::DragValue::new(&mut value)
                    .speed(speed)
                    .range(min..=max),
            )
            .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
    }

    fn u64_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str, min: u64, max: u64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let changed = row(ui, label, |ui| {
            ui.add(egui::DragValue::new(&mut value).speed(1.0).range(min..=max))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
    }

    fn i64_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let changed = row(ui, label, |ui| {
            ui.add(egui::DragValue::new(&mut value).speed(1.0))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
    }

    fn combo_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str, options: &[&str]) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old = value.clone();
        let changed = row(ui, label, |ui| {
            let mut changed = false;
            ComboBox::from_id_salt(path)
                .selected_text(&value)
                .show_ui(ui, |ui| {
                    for option in options {
                        changed |= ui
                            .selectable_value(&mut value, (*option).to_string(), *option)
                            .changed();
                    }
                });
            changed
        });
        if changed || value != old {
            set_json_path(&mut self.model, path, Value::String(value));
        }
    }

    fn list_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let value = get_path(&self.model, path)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let mut text = value;
        let changed = row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(320.0))
                .changed()
        });
        if changed {
            let values = text
                .split(',')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| Value::String(part.to_string()))
                .collect::<Vec<_>>();
            set_json_path(&mut self.model, path, Value::Array(values));
        }
    }

    fn length_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let value = get_path(&self.model, path)
            .map(|value| match value {
                Value::String(value) => value.clone(),
                Value::Number(value) => value.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();
        let mut text = value;
        let changed = row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(150.0))
                .changed()
        });
        if changed {
            let text = text.trim().to_string();
            let value = if text.ends_with('%') {
                Value::String(text)
            } else {
                text.parse::<f64>()
                    .map(|value| json!(value))
                    .unwrap_or(Value::String(text))
            };
            set_json_path(&mut self.model, path, value);
        }
    }

    fn color_field(&mut self, ui: &mut egui::Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = row(ui, label, |ui| {
            let changed = ui
                .add(egui::TextEdit::singleline(&mut value).desired_width(150.0))
                .changed();
            ui.label(RichText::new("■").color(parse_color(&value)));
            changed
        });
        if changed {
            set_json_path(&mut self.model, path, Value::String(value));
        }
    }

    fn bindings_ui(&mut self, ui: &mut egui::Ui, path: &str, actions: &[&str], title: &str) {
        ui.add_space(8.0);
        ui.heading(title);
        let entries = get_path(&self.model, path)
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(code, action)| (code.clone(), action.as_str().unwrap_or("").to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (code, action) in entries {
            let mut next_code = code.clone();
            let mut next_action = action.clone();
            let mut remove = false;
            let changed = row(ui, &code, |ui| {
                let code_changed = ui
                    .add(egui::TextEdit::singleline(&mut next_code).desired_width(130.0))
                    .changed();
                let mut action_changed = false;
                ComboBox::from_id_salt(("binding", &code))
                    .selected_text(&next_action)
                    .show_ui(ui, |ui| {
                        for option in actions {
                            action_changed |= ui
                                .selectable_value(&mut next_action, (*option).to_string(), *option)
                                .changed();
                        }
                    });
                remove = ui.small_button("x").clicked();
                code_changed || action_changed
            });
            if remove {
                remove_map_key(&mut self.model, path, &code);
            } else if next_code != code {
                remove_map_key(&mut self.model, path, &code);
                set_map_value(&mut self.model, path, &next_code, &next_action);
            } else if changed || next_action != action {
                set_map_value(&mut self.model, path, &code, &next_action);
            }
        }
        if ui.button("Add binding").clicked() {
            let mut code = "BTN_NEW".to_string();
            let mut suffix = 1;
            while get_path(&self.model, path)
                .and_then(Value::as_object)
                .is_some_and(|map| map.contains_key(&code))
            {
                code = format!("BTN_NEW{suffix}");
                suffix += 1;
            }
            set_map_value(&mut self.model, path, &code, actions[0]);
        }
    }

    fn layers_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.heading("Layers");
        let names = self.model["gamepad"]["navi"]["layers"]
            .as_object()
            .map(|layers| layers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut remove_layer = None;
        for name in names {
            let path = format!("gamepad.navi.layers.{name}");
            egui::CollapsingHeader::new(&name)
                .id_salt(("layer", &name))
                .show(ui, |ui| {
                    self.list_field(ui, &format!("{path}.hold"), "Hold buttons");
                    let axes_count = self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"]
                        .as_array()
                        .map_or(0, Vec::len);
                    let mut remove_axis = None;
                    for index in 0..axes_count {
                        let axis_path = format!("{path}.hold_axes.{index}");
                        ui.horizontal(|ui| {
                            let mut axis = get_path(&self.model, &format!("{axis_path}.axis"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            if ui
                                .add(egui::TextEdit::singleline(&mut axis).desired_width(90.0))
                                .changed()
                            {
                                set_json_path(
                                    &mut self.model,
                                    &format!("{axis_path}.axis"),
                                    Value::String(axis),
                                );
                            }
                            let mut threshold =
                                get_path(&self.model, &format!("{axis_path}.threshold"))
                                    .and_then(Value::as_f64)
                                    .unwrap_or(0.0);
                            if ui
                                .add(
                                    egui::DragValue::new(&mut threshold)
                                        .speed(0.001)
                                        .range(0.0..=1.0),
                                )
                                .changed()
                            {
                                set_json_path(
                                    &mut self.model,
                                    &format!("{axis_path}.threshold"),
                                    json!(threshold),
                                );
                            }
                            if ui.small_button("x").clicked() {
                                remove_axis = Some(index);
                            }
                        });
                    }
                    if let Some(index) = remove_axis
                        && let Some(axes) =
                            self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"]
                                .as_array_mut()
                    {
                        axes.remove(index);
                    }
                    if ui.button("Add hold axis").clicked()
                        && let Some(axes) =
                            self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"]
                                .as_array_mut()
                    {
                        axes.push(json!({"axis": "ABS_Z", "threshold": 0.35}));
                    }
                    self.bindings_ui(ui, &format!("{path}.bindings"), ACTIONS, "Bindings");
                    if ui.button("Delete layer").clicked() {
                        remove_layer = Some(name.clone());
                    }
                });
        }
        if let Some(name) = remove_layer
            && let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object_mut()
        {
            layers.remove(&name);
        }
        if ui.button("Add layer").clicked() {
            let mut name = "layer".to_string();
            let mut suffix = 1;
            while self.model["gamepad"]["navi"]["layers"]
                .as_object()
                .is_some_and(|layers| layers.contains_key(&name))
            {
                name = format!("layer{suffix}");
                suffix += 1;
            }
            let layers = ensure_object_path(&mut self.model, "gamepad.navi.layers");
            layers.insert(name, json!({"hold": [], "hold_axes": [], "bindings": {}}));
        }
    }

    fn placements_ui(&mut self, ui: &mut egui::Ui) {
        let count = self.model["popup"]["placements"]
            .as_array()
            .map_or(0, Vec::len);
        let mut remove = None;
        for index in 0..count {
            let output = self.model["popup"]["placements"][index]["output"]
                .as_str()
                .unwrap_or("")
                .to_string();
            egui::CollapsingHeader::new(format!("Placement {} {output}", index + 1))
                .id_salt(("placement", index))
                .show(ui, |ui| {
                    let base = format!("popup.placements.{index}");
                    self.text_field(ui, &format!("{base}.output"), "Output");
                    self.combo_field(ui, &format!("{base}.x"), "X", &["left", "center", "right"]);
                    self.combo_field(ui, &format!("{base}.y"), "Y", &["top", "center", "bottom"]);
                    self.length_field(ui, &format!("{base}.margin"), "Margin");
                    if ui.button("Delete placement").clicked() {
                        remove = Some(index);
                    }
                });
        }
        if let Some(index) = remove
            && let Some(items) = self.model["popup"]["placements"].as_array_mut()
        {
            items.remove(index);
        }
        if ui.button("Add placement").clicked()
            && let Some(items) = self.model["popup"]["placements"].as_array_mut()
        {
            items.push(json!({"output": "", "x": "center", "y": "center", "margin": 48.0}));
        }
    }

    fn xmb_preview(&self, ui: &mut egui::Ui) {
        let width = resolve_length(self.model["popup"]["width"].clone(), 920.0).clamp(280.0, 640.0);
        let height =
            resolve_length(self.model["popup"]["height"].clone(), 250.0).clamp(140.0, 300.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter();
        let style = &self.model["style"];
        let background = style_color(style["background"].as_str().unwrap_or(""));
        let background_end = style_color(style["background_end"].as_str().unwrap_or(""));
        let border = style_color(style["border"].as_str().unwrap_or(""));
        let surface = style_color(style["surface"].as_str().unwrap_or(""));
        let surface_hover = style_color(style["surface_hover"].as_str().unwrap_or(""));
        let selection = style_color(style["selection_border"].as_str().unwrap_or(""));
        let accent = style_color(style["accent"].as_str().unwrap_or(""));
        let text = style_color(style["text"].as_str().unwrap_or(""));
        let shadow = style_color(style["shadow"].as_str().unwrap_or("#55000000"));
        let radius = style["panel_radius"].as_f64().unwrap_or(20.0) as u8;
        let border_width = style["border_width"].as_f64().unwrap_or(1.0) as f32;
        let shadow_offset = style["shadow_offset"].as_f64().unwrap_or(6.0) as f32;
        let shadow_spread = style["shadow_spread"].as_f64().unwrap_or(4.0) as f32;
        painter.rect_filled(
            rect.translate(Vec2::new(0.0, shadow_offset))
                .expand(shadow_spread),
            radius,
            shadow,
        );
        for index in 0..12 {
            let t = index as f32 / 11.0;
            let color = lerp_color(background, background_end, t);
            let band = Rect::from_min_max(
                Pos2::new(rect.min.x, rect.min.y + rect.height() * t),
                Pos2::new(
                    rect.max.x,
                    rect.min.y + rect.height() * (t + 1.0 / 11.0).min(1.0),
                ),
            );
            painter.rect_filled(band, 0.0, color);
        }
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(border_width.max(0.5_f32), border),
            StrokeKind::Inside,
        );
        let padding = style["panel_padding"].as_f64().unwrap_or(14.0) as f32;
        let inner = rect.shrink(padding);
        let canvas_gap = style["canvas_gap"].as_f64().unwrap_or(12.0) as f32;
        let canvas_width = (inner.width() - canvas_gap) / 2.0;
        let canvas_height = (inner.height() - 34.0).max(40.0);
        for index in 0..2 {
            let canvas = Rect::from_min_size(
                Pos2::new(
                    inner.min.x + index as f32 * (canvas_width + canvas_gap),
                    inner.min.y,
                ),
                Vec2::new(canvas_width, canvas_height),
            );
            painter.rect_stroke(
                canvas,
                5.0,
                Stroke::new(1.0_f32, border),
                StrokeKind::Inside,
            );
            let icon_size = if index == 0 {
                style["icon_size_focused"].as_f64().unwrap_or(40.0) as f32
            } else {
                style["icon_size"].as_f64().unwrap_or(28.0) as f32
            };
            for item in 0..3 {
                let icon = Rect::from_center_size(
                    Pos2::new(
                        canvas.min.x + 16.0 + item as f32 * (icon_size + 8.0),
                        canvas.min.y + canvas.height() / 2.0,
                    ),
                    Vec2::splat(icon_size),
                );
                let selected = index == 0 && item == 1;
                painter.rect_filled(icon, 4.0, if selected { surface_hover } else { surface });
                painter.rect_stroke(
                    icon,
                    4.0,
                    Stroke::new(1.0_f32, if selected { selection } else { border }),
                    StrokeKind::Inside,
                );
            }
            if self.model["popup"]["show_labels"]
                .as_bool()
                .unwrap_or(false)
            {
                painter.text(
                    Pos2::new(canvas.min.x + 4.0, canvas.min.y + 3.0),
                    Align2::LEFT_TOP,
                    format!("WS {}", index + 1),
                    FontId::monospace(8.0),
                    text,
                );
            }
        }
        painter.text(
            Pos2::new(inner.min.x, inner.max.y - 12.0),
            Align2::LEFT_BOTTOM,
            format!(
                "{} / {}",
                self.model["popup"]["position"].as_str().unwrap_or(""),
                self.model["popup"]["surface_mode"].as_str().unwrap_or("")
            ),
            FontId::monospace(8.0),
            text,
        );
        painter.circle_filled(Pos2::new(inner.max.x - 8.0, inner.max.y - 8.0), 3.0, accent);
    }

    fn bar_preview(&self, ui: &mut egui::Ui) {
        if !self.model["bars_enabled"].as_bool().unwrap_or(false) {
            ui.label("Built-in bars disabled");
            return;
        }
        let Some(bar) = self.model["bars"].as_array().and_then(|bars| bars.first()) else {
            ui.label("No bar entries");
            return;
        };
        let style = &self.model["style"];
        let width = bar["width"].as_f64().unwrap_or(420.0).clamp(200.0, 640.0) as f32;
        let height = bar["height"].as_f64().unwrap_or(38.0).clamp(18.0, 80.0) as f32;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let painter = ui.painter();
        let background = style_color(style["background"].as_str().unwrap_or(""));
        let border = style_color(style["border"].as_str().unwrap_or(""));
        let surface = style_color(style["surface"].as_str().unwrap_or(""));
        let hover = style_color(style["surface_hover"].as_str().unwrap_or(""));
        let accent = style_color(style["accent"].as_str().unwrap_or(""));
        painter.rect_filled(rect, 8.0, background);
        painter.rect_stroke(rect, 8.0, Stroke::new(1.0_f32, border), StrokeKind::Inside);
        for index in 0..4 {
            let center = Pos2::new(rect.min.x + 18.0 + index as f32 * 24.0, rect.center().y);
            painter.circle_filled(center, 7.0, if index == 0 { hover } else { surface });
            if bar["show_capsule"].as_bool().unwrap_or(false) {
                let capsule = Rect::from_min_size(
                    Pos2::new(rect.max.x - 58.0, rect.center().y - 5.0),
                    Vec2::new(28.0, 10.0),
                );
                painter.rect_filled(capsule, 5.0, accent);
            }
        }
        if bar["show_active_indicator"].as_bool().unwrap_or(false) {
            painter.circle_filled(Pos2::new(rect.max.x - 18.0, rect.center().y), 3.0, accent);
        }
    }

    fn confirm_modal(&mut self, ctx: &egui::Context) {
        let Some(confirm) = self.confirm else {
            return;
        };
        let mut action = None;
        egui::Window::new("Confirm")
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(if matches!(confirm, Confirm::Restart) {
                    "Save and restart niri-xmb?"
                } else {
                    "Save this TOML?"
                });
                ui.monospace(&self.config_path);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        action = Some(None);
                    }
                    if ui.button("Save").clicked() {
                        action = Some(Some(matches!(confirm, Confirm::Restart)));
                    }
                });
            });
        if let Some(restart) = action {
            self.confirm = None;
            if let Some(restart) = restart {
                self.save(restart);
            }
        }
    }
}

impl eframe::App for StudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_texture(ctx);
        self.top_bar(ctx);
        egui::SidePanel::left("controller-panel")
            .resizable(false)
            .exact_width(620.0)
            .show(ctx, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    self.controller_panel(ui);
                });
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Controller, "Controller");
                ui.selectable_value(&mut self.tab, Tab::Xmb, "XMB");
                ui.selectable_value(&mut self.tab, Tab::Style, "Style");
                ui.selectable_value(&mut self.tab, Tab::Bars, "Bars");
                ui.selectable_value(&mut self.tab, Tab::Preview, "Preview");
                ui.selectable_value(&mut self.tab, Tab::Save, "Save");
            });
            ui.separator();
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.tab {
                    Tab::Controller => self.controller_tab(ui),
                    Tab::Xmb => self.xmb_tab(ui),
                    Tab::Style => self.style_tab(ui),
                    Tab::Bars => self.bars_tab(ui),
                    Tab::Preview => self.preview_tab(ui),
                    Tab::Save => self.save_tab(ui),
                });
        });
        self.confirm_modal(ctx);
    }
}

fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> bool) -> bool {
    ui.horizontal(|ui| {
        ui.add_sized([190.0, 18.0], egui::Label::new(label));
        add(ui)
    })
    .inner
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        if let Ok(index) = part.parse::<usize>() {
            current = current.as_array()?.get(index)?;
        } else {
            current = current.as_object()?.get(part)?;
        }
    }
    Some(current)
}

fn set_json_path(value: &mut Value, path: &str, next: Value) {
    let parts = path.split('.').collect::<Vec<_>>();
    let mut current = value;
    for part in &parts[..parts.len().saturating_sub(1)] {
        if let Ok(index) = part.parse::<usize>() {
            let Some(array) = current.as_array_mut() else {
                return;
            };
            if array.len() <= index {
                array.resize(index + 1, Value::Null);
            }
            current = &mut array[index];
        } else {
            let Some(object) = current.as_object_mut() else {
                return;
            };
            current = object
                .entry((*part).to_string())
                .or_insert_with(|| Value::Object(Map::new()));
        }
    }
    if let Some(last) = parts.last() {
        if let Ok(index) = last.parse::<usize>() {
            if let Some(array) = current.as_array_mut() {
                if array.len() <= index {
                    array.resize(index + 1, Value::Null);
                }
                array[index] = next;
            }
        } else if let Some(object) = current.as_object_mut() {
            object.insert((*last).to_string(), next);
        }
    }
}

fn ensure_object_path<'a>(value: &'a mut Value, path: &str) -> &'a mut Map<String, Value> {
    let mut current = value;
    for part in path.split('.') {
        if current.as_object().is_none() {
            *current = Value::Object(Map::new());
        }
        current = current
            .as_object_mut()
            .expect("object")
            .entry(part.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
    }
    current.as_object_mut().expect("object")
}

fn binding_path(context: &str) -> String {
    match context {
        "base" => "gamepad.navi.bindings".to_string(),
        "xmb" => "gamepad.buttons".to_string(),
        layer => format!("gamepad.navi.layers.{layer}.bindings"),
    }
}

fn set_map_value(model: &mut Value, path: &str, code: &str, action: &str) {
    ensure_object_path(model, path).insert(code.to_string(), Value::String(action.to_string()));
}

fn remove_map_key(model: &mut Value, path: &str, code: &str) {
    if let Some(map) = get_path_mut(model, path).and_then(Value::as_object_mut) {
        map.remove(code);
    }
}

fn get_path_mut<'a>(value: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let mut current = value;
    for part in path.split('.') {
        current = current.as_object_mut()?.get_mut(part)?;
    }
    Some(current)
}

#[derive(Default)]
struct Patch {
    set: Map<String, Value>,
    remove: Vec<String>,
}

impl Patch {
    fn is_empty(&self) -> bool {
        self.set.is_empty() && self.remove.is_empty()
    }

    fn to_json(&self) -> Value {
        json!({"set": self.set, "remove": self.remove})
    }
}

fn compute_patch(before: &Value, after: &Value) -> Patch {
    let mut patch = Patch::default();
    diff(before, after, "", &mut patch);
    patch
}

fn diff(before: &Value, after: &Value, path: &str, patch: &mut Patch) {
    if before == after {
        return;
    }
    if after.is_null() {
        if !path.is_empty() {
            patch.remove.push(path.to_string());
        }
        return;
    }
    if before.is_array() || after.is_array() {
        patch.set.insert(path.to_string(), after.clone());
        return;
    }
    if let (Some(before), Some(after)) = (before.as_object(), after.as_object()) {
        let keys = before
            .keys()
            .chain(after.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for key in keys {
            let child = if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            let old = before.get(&key);
            let new = after.get(&key);
            if path == "gamepad.navi.layers"
                || path == "gamepad.buttons"
                || path == "gamepad.navi.bindings"
                || path.ends_with(".bindings")
            {
                if new.is_none() {
                    patch.remove.push(child);
                } else if old != new {
                    patch.set.insert(child, new.cloned().unwrap_or(Value::Null));
                }
            } else {
                diff(
                    old.unwrap_or(&Value::Null),
                    new.unwrap_or(&Value::Null),
                    &child,
                    patch,
                );
            }
        }
        return;
    }
    patch.set.insert(path.to_string(), after.clone());
}

fn default_bar() -> Value {
    json!({
        "output": "",
        "edge": "top",
        "align": "start",
        "width": 420,
        "height": 38,
        "margin": 4,
        "offset": 8,
        "allow_column_join": false,
        "show_workspace_labels": false,
        "hide_empty_workspaces": false,
        "show_capsule": true,
        "show_active_indicator": true,
        "scroll_to_switch": true,
        "scroll_wrap": true,
        "scroll_reverse": false,
        "scroll_repeat_ms": 75
    })
}

fn resolve_length(value: Value, total: f32) -> f32 {
    match value {
        Value::Number(value) => value.as_f64().unwrap_or(total as f64) as f32,
        Value::String(value) if value.ends_with('%') => value
            .trim_end_matches('%')
            .parse::<f32>()
            .map_or(total, |percent| total * percent / 100.0),
        Value::String(value) => value.parse::<f32>().unwrap_or(total),
        _ => total,
    }
}

fn style_color(value: &str) -> Color32 {
    let digits = value.strip_prefix('#').unwrap_or("");
    let parse = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).unwrap_or(0);
    match digits.len() {
        6 => Color32::from_rgb(parse(0), parse(2), parse(4)),
        8 => Color32::from_rgba_unmultiplied(parse(2), parse(4), parse(6), parse(0)),
        _ => Color32::TRANSPARENT,
    }
}

fn parse_color(value: &str) -> Color32 {
    style_color(value)
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        mix(a.r(), b.r()),
        mix(a.g(), b.g()),
        mix(a.b(), b.b()),
        mix(a.a(), b.a()),
    )
}

fn main() -> Result<()> {
    let (config, niri_xmb) = parse_args()?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("niri-xmb-config-studio")
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([1100.0, 700.0]),
        ..Default::default()
    };
    eframe::run_native(
        "niri-xmb config",
        options,
        Box::new(move |cc| Ok(Box::new(StudioApp::new(cc, config, niri_xmb)))),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    Ok(())
}

fn parse_args() -> Result<(PathBuf, PathBuf)> {
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut niri_xmb = PathBuf::from(default_niri_xmb());
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = PathBuf::from(args.next().context("--config needs a path")?),
            "--niri-xmb" => {
                niri_xmb = PathBuf::from(args.next().context("--niri-xmb needs a path")?)
            }
            "-h" | "--help" => {
                println!(
                    "usage: niri-xmb-config-studio [--config PATH] [--niri-xmb PATH]\n\
                     Native TOML editor. Validation and save use the niri-xmb binary."
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?}"),
        }
    }
    Ok((expand_path(&config), expand_path(&niri_xmb)))
}

fn default_niri_xmb() -> String {
    let installed = dirs_home().join(".local/bin/niri-xmb");
    if installed.is_file() {
        installed.display().to_string()
    } else {
        "target/debug/niri-xmb".into()
    }
}

fn dirs_home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn expand_path(path: &Path) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix("~/") {
        dirs_home().join(rest)
    } else {
        path.to_path_buf()
    }
}

fn load_config(niri_xmb: &Path, payload: &Value) -> Value {
    let path = payload.get("path").and_then(Value::as_str).unwrap_or("");
    let path = expand_path(Path::new(path));
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return json!({"ok": false, "error": format!("{}: {error}", path.display())});
        }
    };
    match run_niri(
        niri_xmb,
        &["--config", &path.to_string_lossy(), "config-json"],
    ) {
        Ok(output) => match serde_json::from_str::<Value>(&output) {
            Ok(config) => json!({
                "ok": true,
                "path": path.display().to_string(),
                "text": text,
                "sha256": sha256(&text),
                "config": config,
            }),
            Err(error) => {
                json!({"ok": false, "error": format!("config-json returned invalid JSON: {error}")})
            }
        },
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn validate_patch(niri_xmb: &Path, payload: &Value, save: bool) -> Value {
    let Some(path) = payload.get("path").and_then(Value::as_str) else {
        return json!({"ok": false, "error": "missing path"});
    };
    let path = expand_path(Path::new(path));
    let current = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            return json!({"ok": false, "error": format!("{}: {error}", path.display())});
        }
    };
    if let Some(expected) = payload.get("sha256").and_then(Value::as_str)
        && sha256(&current) != expected
    {
        return json!({
            "ok": false,
            "conflict": true,
            "error": "config changed on disk since it was loaded; reload before saving",
        });
    }
    let Some(patch) = payload.get("patch") else {
        return json!({"ok": false, "error": "missing patch"});
    };
    let candidate = match apply_patch(&current, patch) {
        Ok(candidate) => candidate,
        Err(error) => return json!({"ok": false, "error": error.to_string()}),
    };
    let temp = temp_path(&path);
    let _ = fs::remove_file(&temp);
    if let Err(error) = write_and_validate(niri_xmb, &temp, &candidate) {
        let _ = fs::remove_file(&temp);
        return json!({"ok": false, "error": error, "candidate": candidate});
    }
    if !save {
        let _ = fs::remove_file(&temp);
        return json!({"ok": true, "candidate": candidate, "sha256": sha256(&candidate)});
    }
    match replace_atomically(&path, &temp) {
        Ok(()) => json!({
            "ok": true,
            "path": path.display().to_string(),
            "sha256": sha256(&candidate),
            "candidate": candidate,
        }),
        Err(error) => {
            let _ = fs::remove_file(&temp);
            json!({"ok": false, "error": error.to_string()})
        }
    }
}

fn restart(niri_xmb: &Path, payload: &Value) -> Value {
    let Some(path) = payload.get("path").and_then(Value::as_str) else {
        return json!({"ok": false, "error": "missing path"});
    };
    let path = expand_path(Path::new(path));
    match run_niri(niri_xmb, &["--config", &path.to_string_lossy(), "restart"]) {
        Ok(output) => json!({"ok": true, "output": output}),
        Err(error) => json!({"ok": false, "error": error}),
    }
}

fn run_niri(niri_xmb: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new(niri_xmb)
        .args(args)
        .output()
        .map_err(|error| format!("{}: {error}", niri_xmb.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(if stderr.is_empty() { stdout } else { stderr })
    }
}

fn write_and_validate(niri_xmb: &Path, temp: &Path, text: &str) -> Result<(), String> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    use std::io::Write;
    file.write_all(text.as_bytes())
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    file.sync_all()
        .map_err(|error| format!("{}: {error}", temp.display()))?;
    run_niri(
        niri_xmb,
        &["--config", &temp.to_string_lossy(), "config-check"],
    )?;
    Ok(())
}

fn replace_atomically(path: &Path, temp: &Path) -> Result<()> {
    let metadata = fs::metadata(path).with_context(|| format!("{}", path.display()))?;
    fs::set_permissions(temp, metadata.permissions())
        .with_context(|| format!("{}", temp.display()))?;
    fs::rename(temp, path).with_context(|| format!("{}", path.display()))?;
    if let Some(parent) = path.parent()
        && let Ok(dir) = fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }
    Ok(())
}

fn temp_path(path: &Path) -> PathBuf {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(".{file}.studio-{}.tmp", std::process::id()))
}

fn sha256(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn apply_patch(source: &str, patch: &Value) -> Result<String> {
    let mut document = source.parse::<toml_edit::DocumentMut>()?;
    if let Some(remove) = patch.get("remove").and_then(Value::as_array) {
        for path in remove.iter().filter_map(Value::as_str) {
            remove_path(&mut document, path);
        }
    }
    let Some(set) = patch.get("set").and_then(Value::as_object) else {
        return Ok(document.to_string());
    };
    for (path, value) in set {
        set_path(&mut document, path, value)?;
    }
    Ok(document.to_string())
}

fn set_path(document: &mut toml_edit::DocumentMut, path: &str, value: &Value) -> Result<()> {
    let (parent, key) = split_path(path)?;
    if path == "bars" || path == "popup.placements" {
        let item = array_of_tables(value)?;
        set_table_item(document.as_table_mut(), &parent, key, item)?;
        return Ok(());
    }
    let item = json_item(value)?;
    set_table_item(document.as_table_mut(), &parent, key, item)
}

fn split_path(path: &str) -> Result<(Vec<&str>, &str)> {
    let mut parts = path.split('.').collect::<Vec<_>>();
    let key = parts.pop().context("empty patch path")?;
    if key.is_empty() || parts.iter().any(|part| part.is_empty()) {
        anyhow::bail!("invalid patch path {path:?}");
    }
    Ok((parts, key))
}

fn table_mut<'a>(
    table: &'a mut toml_edit::Table,
    parts: &[&str],
) -> Result<&'a mut toml_edit::Table> {
    let mut current = table;
    for part in parts {
        if !current.contains_key(part) {
            current.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let item = current
            .get_mut(part)
            .with_context(|| format!("missing table {part}"))?;
        current = item
            .as_table_mut()
            .with_context(|| format!("{part} is not a table"))?;
    }
    Ok(current)
}

fn set_table_item(
    root: &mut toml_edit::Table,
    parent: &[&str],
    key: &str,
    item: toml_edit::Item,
) -> Result<()> {
    table_mut(root, parent)?.insert(key, item);
    Ok(())
}

fn remove_path(document: &mut toml_edit::DocumentMut, path: &str) {
    let Ok((parent, key)) = split_path(path) else {
        return;
    };
    let Some(root) = find_table_mut(document.as_table_mut(), &parent) else {
        return;
    };
    root.remove(key);
}

fn find_table_mut<'a>(
    table: &'a mut toml_edit::Table,
    parts: &[&str],
) -> Option<&'a mut toml_edit::Table> {
    let mut current = table;
    for part in parts {
        current = current.get_mut(part)?.as_table_mut()?;
    }
    Some(current)
}

fn json_item(value: &Value) -> Result<toml_edit::Item> {
    Ok(match value {
        Value::Object(_) => toml_edit::Item::Table(json_table(value)?),
        Value::Array(_) => toml_edit::Item::Value(json_value(value)?),
        _ => toml_edit::Item::Value(json_value(value)?),
    })
}

fn json_table(value: &Value) -> Result<toml_edit::Table> {
    let object = value.as_object().context("expected object")?;
    let mut table = toml_edit::Table::new();
    for (key, value) in object {
        if value.is_object() {
            table.insert(key, toml_edit::Item::Table(json_table(value)?));
        } else {
            table.insert(key, toml_edit::Item::Value(json_value(value)?));
        }
    }
    Ok(table)
}

fn json_value(value: &Value) -> Result<toml_edit::Value> {
    Ok(match value {
        Value::Null => anyhow::bail!("null is not a TOML value"),
        Value::Bool(value) => (*value).into(),
        Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                integer.into()
            } else {
                value.as_f64().context("invalid number")?.into()
            }
        }
        Value::String(value) => value.as_str().into(),
        Value::Array(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                if value.is_object() {
                    array.push(toml_edit::Value::InlineTable(json_inline_table(value)?));
                } else {
                    array.push(json_value(value)?);
                }
            }
            toml_edit::Value::Array(array)
        }
        Value::Object(_) => toml_edit::Value::InlineTable(json_inline_table(value)?),
    })
}

fn json_inline_table(value: &Value) -> Result<toml_edit::InlineTable> {
    let object = value.as_object().context("expected object")?;
    let mut table = toml_edit::InlineTable::new();
    for (key, value) in object {
        table.insert(key, json_value(value)?);
    }
    Ok(table)
}

fn array_of_tables(value: &Value) -> Result<toml_edit::Item> {
    let values = value.as_array().context("expected array")?;
    let mut tables = toml_edit::ArrayOfTables::new();
    for value in values {
        tables.push(json_table(value)?);
    }
    Ok(toml_edit::Item::ArrayOfTables(tables))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_only_changes_edited_leaves_and_binding_entries() {
        let before = json!({
            "popup": {"width": 920, "height": 250},
            "gamepad": {"preset": "auto", "navi": {"bindings": {"BTN_SOUTH": "pointer.left"}}}
        });
        let mut after = before.clone();
        after["popup"]["width"] = json!("15%");
        after["gamepad"]["preset"] = json!("xbox");
        after["gamepad"]["navi"]["bindings"]["BTN_EAST"] = json!("pointer.right");
        after["gamepad"]["navi"]["bindings"]
            .as_object_mut()
            .unwrap()
            .remove("BTN_SOUTH");
        let patch = compute_patch(&before, &after);
        assert_eq!(patch.set["popup.width"], "15%");
        assert_eq!(patch.set["gamepad.preset"], "xbox");
        assert_eq!(patch.set["gamepad.navi.bindings.BTN_EAST"], "pointer.right");
        assert_eq!(patch.remove, vec!["gamepad.navi.bindings.BTN_SOUTH"]);
        assert!(!patch.set.contains_key("popup.height"));
    }

    #[test]
    fn patch_sets_arrays_and_layers_as_whole_values() {
        let before =
            json!({"bars": [], "popup": {"placements": []}, "gamepad": {"navi": {"layers": {}}}});
        let mut after = before.clone();
        after["bars"] = json!([{"output": "eDP-1", "width": 420}]);
        after["popup"]["placements"] =
            json!([{"output": "DP-1", "x": "center", "y": "top", "margin": "4%"}]);
        after["gamepad"]["navi"]["layers"]["workspace"] = json!({"hold": ["BTN_TL"], "hold_axes": [], "bindings": {"BTN_DPAD_LEFT": "niri.focus-workspace-prev"}});
        let patch = compute_patch(&before, &after);
        assert_eq!(patch.set["bars"], after["bars"]);
        assert_eq!(patch.set["popup.placements"], after["popup"]["placements"]);
        assert_eq!(
            patch.set["gamepad.navi.layers.workspace"],
            after["gamepad"]["navi"]["layers"]["workspace"]
        );
    }

    #[test]
    fn patch_is_empty_for_unchanged_model_and_removes_nulls() {
        let before = json!({"theme": "theme.toml", "gamepad": {"enabled": true}});
        assert!(compute_patch(&before, &before).is_empty());
        let mut after = before.clone();
        after["theme"] = Value::Null;
        let patch = compute_patch(&before, &after);
        assert_eq!(patch.remove, vec!["theme"]);
        assert!(patch.set.is_empty());
    }

    #[test]
    fn patch_preserves_comments_unknown_fields_and_unrelated_tables() {
        let source = r#"# top comment
[popup]
width = 920 # keep this comment
height = 250
unknown_popup_field = "keep me"

[peek]
on_window_focus = true # unknown to the current Rust schema

[gamepad]
enabled = true
preset = "auto"
unknown_gamepad_field = 7
"#;
        let patch = json!({"set": {"popup.width": "15%", "gamepad.preset": "xbox"}, "remove": []});
        let output = apply_patch(source, &patch).unwrap();
        assert!(output.contains("unknown_popup_field = \"keep me\""));
        assert!(output.contains("on_window_focus = true"));
        assert!(output.contains("unknown_gamepad_field = 7"));
        assert!(output.contains("width = \"15%\""));
        assert!(output.contains("preset = \"xbox\""));
        assert!(output.contains("# top comment"));
    }

    #[test]
    fn patch_sets_bars_and_placements_as_tables() {
        let source = "[popup]\nwidth = 920\n";
        let patch = json!({
            "set": {
                "bars": [{"output": "eDP-1", "edge": "top", "align": "start", "width": 420, "height": 38}],
                "popup.placements": [{"output": "HDMI-A-1", "x": "center", "y": "top", "margin": "4%"}]
            },
            "remove": []
        });
        let output = apply_patch(source, &patch).unwrap();
        assert!(output.contains("[[bars]]"));
        assert!(output.contains("[[popup.placements]]"));
        assert!(output.contains("margin = \"4%\""));
    }

    #[test]
    fn patch_removes_binding_without_creating_parent_tables() {
        let source = "[gamepad.navi.bindings]\nBTN_SOUTH = \"pointer.left\"\n";
        let patch =
            json!({"set": {}, "remove": ["gamepad.navi.bindings.BTN_SOUTH", "missing.table.key"]});
        let output = apply_patch(source, &patch).unwrap();
        assert!(!output.contains("BTN_SOUTH"));
        assert!(!output.contains("[missing.table]"));
    }

    #[test]
    fn real_schema_accepts_a_patched_example() {
        let niri = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/niri-xmb");
        if !niri.is_file() {
            eprintln!("skipping real-schema test: build target/debug/niri-xmb first");
            return;
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/niri-xmb-studio-real-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, include_str!("../../../config/niri-xmb.toml.example")).unwrap();
        let loaded = load_config(&niri, &json!({"path": path.display().to_string()}));
        assert_eq!(loaded["ok"], true, "{loaded}");
        let before = fs::read_to_string(&path).unwrap();
        let result = validate_patch(
            &niri,
            &json!({
                "path": path.display().to_string(),
                "sha256": sha256(&before),
                "patch": {"set": {"popup.width": "15%", "style.accent": "#ff112233"}, "remove": []}
            }),
            true,
        );
        assert_eq!(result["ok"], true, "{result}");
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("width = \"15%\""));
        assert!(saved.contains("accent = \"#ff112233\""));
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn atomic_replace_swaps_file_contents() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/niri-xmb-studio-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("config.toml");
        fs::write(&target, "old\n").unwrap();
        let temp = temp_path(&target);
        fs::write(&temp, "new\n").unwrap();
        replace_atomically(&target, &temp).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new\n");
        assert!(!temp.exists());
        fs::remove_dir_all(&dir).ok();
    }
}
