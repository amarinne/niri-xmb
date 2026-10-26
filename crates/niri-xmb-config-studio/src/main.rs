//! Native editor for niri-xmb TOML.
//!
//! Left navigation selects a section, the command bar loads, validates and
//! saves, and a persistent preview pane paints the current model. The editor
//! loads the real schema through `niri-xmb config-json`, writes only changed
//! paths through `toml_edit`, validates with `config-check`, and saves
//! atomically. It does not talk to the daemon except through the existing
//! `restart` command after an explicit Save & Restart.

use anyhow::{Context, Result};
use eframe::egui;
use egui::{
    Align, Align2, Color32, ComboBox, FontId, Layout, Pos2, Rect, RichText, ScrollArea, Sense,
    Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_CONFIG: &str = "~/.config/niri-xmb/config.toml";
const XBOX_IMAGE: &[u8] = include_bytes!("../../../assets/controller-render.png");
const DS4_IMAGE: &[u8] = include_bytes!("../../../assets/dualshock4-render.png");
const XBOX_ASPECT: f32 = 861.0 / 1280.0;
const DS4_ASPECT: f32 = 750.0 / 1387.0;
const DS4_BACKDROP: Color32 = Color32::from_rgb(226, 229, 234);

const BG: Color32 = Color32::from_rgb(16, 18, 22);
const PANEL: Color32 = Color32::from_rgb(20, 23, 28);
const CARD: Color32 = Color32::from_rgb(25, 29, 35);
const FIELD_BG: Color32 = Color32::from_rgb(13, 15, 19);
const BORDER: Color32 = Color32::from_rgb(42, 48, 58);
const NAV_SELECTED: Color32 = Color32::from_rgb(33, 42, 58);
const ACCENT: Color32 = Color32::from_rgb(116, 162, 255);
const TEXT: Color32 = Color32::from_rgb(226, 231, 240);
const TEXT_MUTED: Color32 = Color32::from_rgb(142, 151, 166);
const TEXT_DIM: Color32 = Color32::from_rgb(100, 109, 124);
const OK: Color32 = Color32::from_rgb(118, 196, 146);
const WARN: Color32 = Color32::from_rgb(230, 170, 70);
const ERROR: Color32 = Color32::from_rgb(232, 112, 112);

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

const COLOR_FIELDS: &[(&str, &str)] = &[
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
];

const GEOMETRY_FIELDS: &[(&str, &str, f64, f64, f64)] = &[
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
];

struct Hotspot {
    control: &'static str,
    code: Option<&'static str>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    circle: bool,
    label: bool,
}

const XBOX_HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "LT",
        code: None,
        x: 0.247,
        y: 0.142,
        w: 0.070,
        h: 0.080,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "RT",
        code: None,
        x: 0.753,
        y: 0.142,
        w: 0.070,
        h: 0.080,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "LB",
        code: Some("BTN_TL"),
        x: 0.213,
        y: 0.231,
        w: 0.155,
        h: 0.050,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "RB",
        code: Some("BTN_TR"),
        x: 0.784,
        y: 0.231,
        w: 0.150,
        h: 0.050,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "L STICK",
        code: None,
        x: 0.223,
        y: 0.530,
        w: 0.105,
        h: 0.105,
        circle: true,
        label: true,
    },
    Hotspot {
        control: "D-PAD",
        code: None,
        x: 0.355,
        y: 0.706,
        w: 0.170,
        h: 0.145,
        circle: false,
        label: true,
    },
    Hotspot {
        control: "R STICK",
        code: None,
        x: 0.628,
        y: 0.722,
        w: 0.120,
        h: 0.120,
        circle: true,
        label: true,
    },
    Hotspot {
        control: "Y",
        code: Some("BTN_NORTH"),
        x: 0.779,
        y: 0.409,
        w: 0.075,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "X",
        code: Some("BTN_WEST"),
        x: 0.701,
        y: 0.501,
        w: 0.075,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "B",
        code: Some("BTN_EAST"),
        x: 0.850,
        y: 0.492,
        w: 0.075,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "A",
        code: Some("BTN_SOUTH"),
        x: 0.772,
        y: 0.586,
        w: 0.075,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "SELECT",
        code: Some("BTN_SELECT"),
        x: 0.378,
        y: 0.487,
        w: 0.052,
        h: 0.060,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "START",
        code: Some("BTN_START"),
        x: 0.567,
        y: 0.487,
        w: 0.052,
        h: 0.060,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "GUIDE",
        code: Some("BTN_MODE"),
        x: 0.500,
        y: 0.490,
        w: 0.085,
        h: 0.085,
        circle: true,
        label: false,
    },
];

const DS4_HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "Touchpad",
        code: None,
        x: 0.500,
        y: 0.390,
        w: 0.360,
        h: 0.160,
        circle: false,
        label: true,
    },
    Hotspot {
        control: "L2",
        code: Some("BTN_TL2"),
        x: 0.223,
        y: 0.048,
        w: 0.100,
        h: 0.055,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "R2",
        code: Some("BTN_TR2"),
        x: 0.843,
        y: 0.048,
        w: 0.100,
        h: 0.055,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "L1",
        code: Some("BTN_TL"),
        x: 0.225,
        y: 0.122,
        w: 0.100,
        h: 0.050,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "R1",
        code: Some("BTN_TR"),
        x: 0.842,
        y: 0.122,
        w: 0.100,
        h: 0.050,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "Triangle",
        code: Some("BTN_NORTH"),
        x: 0.806,
        y: 0.396,
        w: 0.070,
        h: 0.070,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Circle",
        code: Some("BTN_EAST"),
        x: 0.880,
        y: 0.508,
        w: 0.070,
        h: 0.070,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Cross",
        code: Some("BTN_SOUTH"),
        x: 0.807,
        y: 0.620,
        w: 0.070,
        h: 0.070,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Square",
        code: Some("BTN_WEST"),
        x: 0.721,
        y: 0.511,
        w: 0.070,
        h: 0.070,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "D-PAD",
        code: None,
        x: 0.182,
        y: 0.505,
        w: 0.170,
        h: 0.140,
        circle: false,
        label: true,
    },
    Hotspot {
        control: "L3",
        code: Some("BTN_THUMBL"),
        x: 0.349,
        y: 0.789,
        w: 0.110,
        h: 0.110,
        circle: true,
        label: true,
    },
    Hotspot {
        control: "R3",
        code: Some("BTN_THUMBR"),
        x: 0.660,
        y: 0.789,
        w: 0.110,
        h: 0.110,
        circle: true,
        label: true,
    },
    Hotspot {
        control: "Share",
        code: Some("BTN_SELECT"),
        x: 0.292,
        y: 0.319,
        w: 0.045,
        h: 0.060,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "Options",
        code: Some("BTN_START"),
        x: 0.706,
        y: 0.319,
        w: 0.045,
        h: 0.060,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "PS",
        code: Some("BTN_MODE"),
        x: 0.500,
        y: 0.681,
        w: 0.070,
        h: 0.070,
        circle: true,
        label: false,
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Template {
    Xbox,
    Ds4,
}

impl Template {
    fn label(self) -> &'static str {
        match self {
            Self::Xbox => "Xbox",
            Self::Ds4 => "DS4",
        }
    }

    fn image(self) -> &'static [u8] {
        match self {
            Self::Xbox => XBOX_IMAGE,
            Self::Ds4 => DS4_IMAGE,
        }
    }

    fn texture_name(self) -> &'static str {
        match self {
            Self::Xbox => "controller-xbox",
            Self::Ds4 => "controller-ds4",
        }
    }

    fn aspect(self) -> f32 {
        match self {
            Self::Xbox => XBOX_ASPECT,
            Self::Ds4 => DS4_ASPECT,
        }
    }

    fn hotspots(self) -> &'static [Hotspot] {
        match self {
            Self::Xbox => XBOX_HOTSPOTS,
            Self::Ds4 => DS4_HOTSPOTS,
        }
    }

    fn backdrop(self) -> Option<Color32> {
        match self {
            Self::Xbox => None,
            Self::Ds4 => Some(DS4_BACKDROP),
        }
    }

    fn default_control(self) -> &'static str {
        match self {
            Self::Xbox => "A",
            Self::Ds4 => "Cross",
        }
    }

    fn counterpart(self, control: &str) -> Option<&'static str> {
        match (self, control) {
            (Self::Xbox, "L2") => Some("LT"),
            (Self::Xbox, "L1") => Some("LB"),
            (Self::Xbox, "R2") => Some("RT"),
            (Self::Xbox, "R1") => Some("RB"),
            (Self::Xbox, "Triangle") => Some("Y"),
            (Self::Xbox, "Circle") => Some("B"),
            (Self::Xbox, "Cross") => Some("A"),
            (Self::Xbox, "Square") => Some("X"),
            (Self::Xbox, "Share") => Some("SELECT"),
            (Self::Xbox, "Options") => Some("START"),
            (Self::Xbox, "PS") => Some("GUIDE"),
            (Self::Xbox, "L3") => Some("L STICK"),
            (Self::Xbox, "R3") => Some("R STICK"),
            (Self::Ds4, "LT") => Some("L2"),
            (Self::Ds4, "LB") => Some("L1"),
            (Self::Ds4, "RT") => Some("R2"),
            (Self::Ds4, "RB") => Some("R1"),
            (Self::Ds4, "Y") => Some("Triangle"),
            (Self::Ds4, "B") => Some("Circle"),
            (Self::Ds4, "A") => Some("Cross"),
            (Self::Ds4, "X") => Some("Square"),
            (Self::Ds4, "SELECT") => Some("Share"),
            (Self::Ds4, "START") => Some("Options"),
            (Self::Ds4, "GUIDE") => Some("PS"),
            (Self::Ds4, "L STICK") => Some("L3"),
            (Self::Ds4, "R STICK") => Some("R3"),
            (_, "D-PAD") => Some("D-PAD"),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Controller,
    Xmb,
    Style,
    Bars,
    Preview,
    Save,
}

impl Section {
    const ALL: [(Section, &'static str, &'static str); 6] = [
        (Self::Controller, "Controller", "Map, triggers, bindings"),
        (Self::Xmb, "XMB", "Panel, behavior, peek"),
        (Self::Style, "Style", "Theme, colors, geometry"),
        (Self::Bars, "Bars", "Status bars"),
        (Self::Preview, "Preview", "Live model preview"),
        (Self::Save, "Save", "Validate, save, restart"),
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Xmb => "XMB",
            Self::Style => "Style",
            Self::Bars => "Bars",
            Self::Preview => "Preview",
            Self::Save => "Save",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Controller => "Map, trigger response and bindings",
            Self::Xmb => "Panel, behavior and peek",
            Self::Style => "Theme, colors and geometry",
            Self::Bars => "Status bar layout",
            Self::Preview => "Painted from the current model",
            Self::Save => "Validate, save and restart",
        }
    }
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
    section: Section,
    show_preview: bool,
    template: Template,
    selected: &'static str,
    context: String,
    status: String,
    error: String,
    candidate: String,
    confirm: Option<Confirm>,
    sim_lt: f32,
    sim_rt: f32,
    xbox_texture: Option<TextureHandle>,
    ds4_texture: Option<TextureHandle>,
}

impl StudioApp {
    fn new(cc: &eframe::CreationContext<'_>, config: PathBuf, niri_xmb: PathBuf) -> Self {
        configure(&cc.egui_ctx);
        let mut app = Self {
            niri_xmb,
            config_path: config.display().to_string(),
            model: Value::Null,
            loaded: Value::Null,
            sha256: String::new(),
            section: Section::Controller,
            show_preview: true,
            template: Template::Xbox,
            selected: Template::Xbox.default_control(),
            context: "base".to_string(),
            status: String::new(),
            error: String::new(),
            candidate: String::new(),
            confirm: None,
            sim_lt: 0.0,
            sim_rt: 0.0,
            xbox_texture: None,
            ds4_texture: None,
        };
        app.load();
        app
    }

    fn ensure_textures(&mut self, ctx: &egui::Context) {
        if self.xbox_texture.is_none() {
            self.xbox_texture = load_texture(ctx, Template::Xbox);
        }
        if self.ds4_texture.is_none() {
            self.ds4_texture = load_texture(ctx, Template::Ds4);
        }
    }

    fn load(&mut self) {
        let result = load_config(&self.niri_xmb, &json!({"path": self.config_path}));
        if result["ok"] != true {
            self.model = Value::Null;
            self.loaded = Value::Null;
            self.sha256.clear();
            self.candidate.clear();
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

    fn revert(&mut self) {
        self.model = self.loaded.clone();
        self.error.clear();
        self.candidate.clear();
        self.status = "Reverted".to_string();
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

    fn command_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("command-bar")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 7))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Config").size(11.0).color(TEXT_DIM));
                    let path_width = (ui.available_width() - 440.0).clamp(180.0, 760.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.config_path)
                            .desired_width(path_width)
                            .hint_text("~/.config/niri-xmb/config.toml"),
                    );
                    if ui.button("Load").clicked() {
                        self.load();
                    }
                    if ui.button("Validate").clicked() {
                        self.validate();
                    }
                    let dirty = self.dirty();
                    ui.add_enabled_ui(dirty, |ui| {
                        if ui.button("Save").clicked() {
                            self.confirm = Some(Confirm::Save);
                        }
                        if ui.button("Save & Restart").clicked() {
                            self.confirm = Some(Confirm::Restart);
                        }
                        if ui.button("Revert").clicked() {
                            self.revert();
                        }
                    });
                });
            });
    }

    fn nav_rail(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav-rail")
            .exact_width(196.0)
            .resizable(false)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 12))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                    ui.painter().rect_filled(rect, 2.0, ACCENT);
                    ui.label(RichText::new("niri-xmb").size(16.0).strong());
                });
                ui.label(RichText::new("Config studio").size(10.5).color(TEXT_DIM));
                ui.add_space(14.0);
                for (section, label, hint) in Section::ALL {
                    if nav_item(ui, self.section == section, label, hint).clicked() {
                        self.section = section;
                    }
                    ui.add_space(2.0);
                }
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    self.nav_status(ui);
                    ui.add_space(8.0);
                    ui.checkbox(&mut self.show_preview, "Preview pane");
                    ui.add_space(6.0);
                    ui.separator();
                });
            });
    }

    fn nav_status(&self, ui: &mut Ui) {
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(7.0)
            .inner_margin(egui::Margin::same(9))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let dot = if !self.error.is_empty() {
                    ERROR
                } else if self.dirty() {
                    WARN
                } else {
                    OK
                };
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, dot);
                    ui.label(
                        RichText::new(if self.dirty() { "Edited" } else { "Clean" })
                            .size(11.5)
                            .color(TEXT),
                    );
                });
                ui.add_space(2.0);
                let status: &str = if !self.error.is_empty() {
                    "Error"
                } else if self.status.is_empty() {
                    "Ready"
                } else {
                    &self.status
                };
                let response = ui.label(RichText::new(status).size(11.0).color(
                    if self.error.is_empty() {
                        status_color(status)
                    } else {
                        ERROR
                    },
                ));
                if !self.error.is_empty() {
                    response.on_hover_text(&self.error);
                }
                ui.label(
                    RichText::new(format!("sha {}", self.short_sha()))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                );
            });
    }

    fn short_sha(&self) -> &str {
        let sha = self.sha256.as_str();
        if sha.is_empty() {
            "-"
        } else {
            sha.get(..10).unwrap_or(sha)
        }
    }

    fn preview_pane(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("preview-pane")
            .default_width(360.0)
            .min_width(300.0)
            .max_width(520.0)
            .resizable(true)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 12))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ScrollArea::vertical()
                    .id_salt("preview-scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Preview").size(15.0).strong());
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Full").clicked() {
                                    self.section = Section::Preview;
                                }
                            });
                        });
                        ui.label(
                            RichText::new("Model preview; renderer authoritative")
                                .size(10.0)
                                .color(TEXT_DIM),
                        );
                        ui.add_space(8.0);

                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("XMB").size(12.0).strong());
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new(format!(
                                            "{} / {}",
                                            self.model["popup"]["position"]
                                                .as_str()
                                                .unwrap_or("cursor"),
                                            self.model["popup"]["surface_mode"]
                                                .as_str()
                                                .unwrap_or("fixed")
                                        ))
                                        .monospace()
                                        .size(9.5)
                                        .color(TEXT_DIM),
                                    );
                                });
                            });
                            ui.add_space(4.0);
                            self.xmb_preview(ui, ui.available_width());
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new(format!(
                                    "{} / {}",
                                    self.model["popup"]["outputs"].as_str().unwrap_or("focused"),
                                    self.model["popup"]["show_titles"]
                                        .as_bool()
                                        .map(|on| if on { "titles" } else { "no titles" })
                                        .unwrap_or("no titles")
                                ))
                                .monospace()
                                .size(9.5)
                                .color(TEXT_DIM),
                            );
                        });

                        card(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new("Bar").size(12.0).strong());
                                if let Some(output) = self.model["bars"]
                                    .as_array()
                                    .and_then(|bars| bars.first())
                                    .and_then(|bar| bar["output"].as_str())
                                    && !output.is_empty()
                                {
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.label(
                                            RichText::new(output)
                                                .monospace()
                                                .size(9.5)
                                                .color(TEXT_DIM),
                                        );
                                    });
                                }
                            });
                            ui.add_space(4.0);
                            if !self.model["bars_enabled"].as_bool().unwrap_or(false) {
                                ui.label(
                                    RichText::new("Bars disabled").size(11.0).color(TEXT_MUTED),
                                );
                            } else if let Some(bar) = self.model["bars"]
                                .as_array()
                                .and_then(|bars| bars.first())
                                .cloned()
                            {
                                self.bar_preview(ui, &bar, ui.available_width());
                            } else {
                                ui.label(RichText::new("No bars").size(11.0).color(TEXT_MUTED));
                            }
                        });

                        card(ui, |ui| {
                            ui.label(RichText::new("Model").size(12.0).strong());
                            ui.add_space(4.0);
                            metric_row(
                                ui,
                                "Navi bindings",
                                map_len(&self.model["gamepad"]["navi"]["bindings"]),
                            );
                            metric_row(
                                ui,
                                "XMB buttons",
                                map_len(&self.model["gamepad"]["buttons"]),
                            );
                            metric_row(
                                ui,
                                "Layers",
                                map_len(&self.model["gamepad"]["navi"]["layers"]),
                            );
                            metric_row(ui, "Bars", array_len(&self.model["bars"]));
                            metric_row(
                                ui,
                                "Placements",
                                array_len(&self.model["popup"]["placements"]),
                            );
                        });
                    });
            });
    }

    fn error_banner(&mut self, ui: &mut Ui) {
        if self.error.is_empty() {
            return;
        }
        egui::Frame::new()
            .fill(Color32::from_rgb(44, 25, 28))
            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(94, 44, 50)))
            .corner_radius(8.0)
            .inner_margin(egui::Margin::same(10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Error").strong().color(ERROR));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.error.clear();
                        }
                    });
                });
                ui.label(
                    RichText::new(&self.error)
                        .size(11.5)
                        .color(Color32::from_rgb(240, 190, 190)),
                );
            });
        ui.add_space(8.0);
    }

    fn content(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ctx, |ui| {
                self.error_banner(ui);
                ScrollArea::vertical()
                    .id_salt("section-scroll")
                    .scroll_source(egui::containers::scroll_area::ScrollSource {
                        drag: false,
                        ..Default::default()
                    })
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.section {
                            Section::Controller => self.controller_section(ui),
                            Section::Xmb => self.xmb_section(ui),
                            Section::Style => self.style_section(ui),
                            Section::Bars => self.bars_section(ui),
                            Section::Preview => self.preview_section(ui),
                            Section::Save => self.save_section(ui),
                        }
                        ui.add_space(24.0);
                    });
            });
    }

    fn section_header(&self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(self.section.title()).size(20.0).strong());
            ui.add_space(2.0);
            ui.label(
                RichText::new(self.section.hint())
                    .size(11.0)
                    .color(TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if self.dirty() {
                    ui.label(RichText::new("Unsaved changes").size(11.0).color(WARN));
                }
            });
        });
        ui.add_space(10.0);
    }

    fn controller_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        if ui.available_width() >= 700.0 {
            ui.columns(2, |cols| {
                self.controls_card(&mut cols[0]);
                self.inspector_card(&mut cols[1]);
            });
        } else {
            self.controls_card(ui);
            self.inspector_card(ui);
        }
        section_card(ui, "Trigger response", |ui| {
            self.trigger_response(ui);
        });
        section_card(ui, "Pedal simulation", |ui| {
            self.pedal_simulation(ui);
        });
        section_card(ui, "Trigger settings", |ui| {
            self.accel_fields(ui);
        });
        section_card(ui, "Device", |ui| {
            self.device_fields(ui);
        });
        section_card(ui, "Pointer", |ui| {
            self.pointer_fields(ui);
        });
        section_card(ui, "Bindings", |ui| {
            self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons");
            ui.add_space(10.0);
            self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base");
        });
        section_card(ui, "Layers", |ui| {
            self.layers_ui(ui);
        });
    }

    fn controls_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Controls").size(12.0).strong());
                ui.add_space(6.0);
                for template in [Template::Xbox, Template::Ds4] {
                    if ui
                        .selectable_label(self.template == template, template.label())
                        .clicked()
                    {
                        self.switch_template(template);
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(RichText::new("Visual only").size(9.5).color(TEXT_DIM));
                });
            });
            ui.add_space(6.0);
            let template = self.template;
            let width = ui.available_width();
            let height = width * template.aspect();
            let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
            if let Some(backdrop) = template.backdrop() {
                ui.painter().rect_filled(rect, 10.0, backdrop);
                ui.painter().rect_stroke(
                    rect,
                    10.0,
                    Stroke::new(1.0_f32, BORDER),
                    StrokeKind::Inside,
                );
            }
            let texture_id = match template {
                Template::Xbox => self.xbox_texture.as_ref(),
                Template::Ds4 => self.ds4_texture.as_ref(),
            }
            .map(TextureHandle::id);
            if let Some(texture_id) = texture_id {
                ui.painter().image(
                    texture_id,
                    rect,
                    Rect::from_min_max(Pos2::new(0.0, 0.0), Pos2::new(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            let label_size = (rect.width() * 0.016).clamp(7.0, 10.0);
            for hotspot in template.hotspots() {
                let center = Pos2::new(
                    rect.min.x + hotspot.x * rect.width(),
                    rect.min.y + hotspot.y * rect.height(),
                );
                let hit = Rect::from_center_size(
                    center,
                    Vec2::new(hotspot.w * rect.width(), hotspot.h * rect.width()),
                );
                let selected = self.selected == hotspot.control;
                let response = ui.interact(
                    hit,
                    ui.id().with(("hotspot", template.label(), hotspot.control)),
                    Sense::click(),
                );
                let stroke = if selected {
                    Stroke::new(2.0_f32, ACCENT)
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
                if hotspot.label {
                    ui.painter().text(
                        Pos2::new(center.x, hit.max.y + 3.0),
                        Align2::CENTER_TOP,
                        hotspot.control,
                        FontId::monospace(label_size),
                        Color32::from_gray(166),
                    );
                }
                if response.clicked() {
                    self.selected = hotspot.control;
                }
            }
        });
    }

    fn switch_template(&mut self, template: Template) {
        if template == self.template {
            return;
        }
        let code = self
            .template
            .hotspots()
            .iter()
            .find(|hotspot| hotspot.control == self.selected)
            .and_then(|hotspot| hotspot.code);
        let next = code
            .and_then(|code| {
                template
                    .hotspots()
                    .iter()
                    .find(|hotspot| hotspot.code == Some(code))
            })
            .map(|hotspot| hotspot.control)
            .or_else(|| template.counterpart(self.selected))
            .unwrap_or_else(|| template.default_control());
        self.template = template;
        self.selected = next;
    }

    fn inspector_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.label(RichText::new("Selected").size(12.0).strong());
            ui.add_space(2.0);
            let code = self
                .template
                .hotspots()
                .iter()
                .find(|hotspot| hotspot.control == self.selected)
                .and_then(|hotspot| hotspot.code);
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.selected).size(18.0).strong());
                if let Some(code) = code {
                    ui.label(RichText::new(code).monospace().size(10.5).color(TEXT_MUTED));
                }
            });
            ui.add_space(6.0);
            if let Some(code) = code {
                self.binding_editor(ui, code);
            }
            if let Some(hint) = control_hint(self.template, self.selected) {
                ui.add_space(4.0);
                ui.label(RichText::new(hint).size(11.5).color(TEXT_MUTED));
            }
        });
    }

    fn binding_editor(&mut self, ui: &mut Ui, code: &str) {
        let mut contexts = vec!["base".to_string(), "xmb".to_string()];
        if let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object() {
            contexts.extend(layers.keys().cloned());
        }
        if !contexts.contains(&self.context) {
            self.context = "base".to_string();
        }
        ui.horizontal(|ui| {
            field_label(ui, "Context");
            ComboBox::from_id_salt("binding-context")
                .width(input_width(ui).clamp(90.0, 240.0))
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
            field_label(ui, "Action");
            ComboBox::from_id_salt("binding-action")
                .width(input_width(ui).clamp(90.0, 260.0))
                .selected_text(if next.is_empty() { "unbound" } else { &next })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next, String::new(), "unbound");
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
        ui.add_space(6.0);
        ui.label(
            RichText::new("Bindings in context")
                .size(10.0)
                .color(TEXT_DIM),
        );
        let map = get_path(&self.model, &map_path)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if map.is_empty() {
            ui.label(RichText::new("No bindings").size(11.0).color(TEXT_MUTED));
        } else {
            ScrollArea::vertical()
                .id_salt("selected-bindings")
                .max_height(150.0)
                .show(ui, |ui| {
                    for (code, action) in map {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(code).monospace().size(10.5));
                            ui.label(RichText::new("→").size(10.5).color(TEXT_DIM));
                            ui.label(
                                RichText::new(action.as_str().unwrap_or(""))
                                    .size(10.5)
                                    .color(TEXT_MUTED),
                            );
                        });
                    }
                });
        }
    }

    fn device_fields(&mut self, ui: &mut Ui) {
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                self.bool_field(&mut cols[0], "gamepad.enabled", "Enabled");
                self.combo_field(
                    &mut cols[0],
                    "gamepad.preset",
                    "Preset",
                    &["none", "auto", "xbox", "ds4"],
                );
                self.list_field(&mut cols[0], "gamepad.devices", "Devices");
                self.list_field(&mut cols[0], "gamepad.open_combo", "Open combo");

                self.combo_field(
                    &mut cols[1],
                    "gamepad.mode.default",
                    "Default mode",
                    &["game", "navi"],
                );
                self.list_field(&mut cols[1], "gamepad.mode.toggle", "Mode toggle");
                self.bool_field(&mut cols[1], "gamepad.mode.grab_in_navi", "Grab in navi");
                self.bool_field(
                    &mut cols[1],
                    "gamepad.mode.reset_on_disconnect",
                    "Reset on disconnect",
                );
            });
        } else {
            self.bool_field(ui, "gamepad.enabled", "Enabled");
            self.combo_field(
                ui,
                "gamepad.preset",
                "Preset",
                &["none", "auto", "xbox", "ds4"],
            );
            self.list_field(ui, "gamepad.devices", "Devices");
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
        }
    }

    fn pointer_fields(&mut self, ui: &mut Ui) {
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                mini_heading(&mut cols[0], "Motion");
                self.bool_field(&mut cols[0], "gamepad.navi.pointer.enabled", "Enabled");
                self.combo_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.stick",
                    "Stick",
                    &["left", "right"],
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.deadzone",
                    "Deadzone",
                    0.0,
                    1.0,
                    0.005,
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.curve",
                    "Curve",
                    0.05,
                    10.0,
                    0.05,
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.speed",
                    "Speed",
                    0.0,
                    20000.0,
                    10.0,
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.max_step",
                    "Max step",
                    0.0,
                    1000.0,
                    1.0,
                );
                self.bool_field(&mut cols[0], "gamepad.navi.pointer.invert_y", "Invert Y");

                mini_heading(&mut cols[1], "Scroll");
                self.combo_field(
                    &mut cols[1],
                    "gamepad.navi.pointer.scroll_stick",
                    "Stick",
                    &["left", "right"],
                );
                self.f32_field(
                    &mut cols[1],
                    "gamepad.navi.pointer.scroll_deadzone",
                    "Deadzone",
                    0.0,
                    1.0,
                    0.005,
                );
                self.f32_field(
                    &mut cols[1],
                    "gamepad.navi.pointer.scroll_speed",
                    "Speed",
                    0.0,
                    1000.0,
                    0.1,
                );
            });
        } else {
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
        }
    }

    fn trigger_response(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Y = speed multiplier (1.0× = baseline). X = trigger pull.")
                .size(10.0)
                .color(TEXT_DIM),
        );
        ui.label(
            RichText::new(
                "LT is the brake (full pull = slower). RT is the gas (full pull = faster).",
            )
            .size(10.0)
            .color(TEXT_DIM),
        );
        ui.add_space(6.0);
        if ui.available_width() >= 620.0 {
            ui.columns(2, |cols| {
                self.trigger_graph(&mut cols[0], "Pointer", "gamepad.navi.pointer.accel");
                self.trigger_graph(&mut cols[1], "Scroll", "gamepad.navi.pointer.scroll_accel");
            });
        } else {
            self.trigger_graph(ui, "Pointer", "gamepad.navi.pointer.accel");
            ui.add_space(6.0);
            self.trigger_graph(ui, "Scroll", "gamepad.navi.pointer.scroll_accel");
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Drag the LT/RT endpoints and the deadzone/curve handles to edit the real settings.")
                .size(9.5)
                .color(TEXT_DIM),
        );
    }

    fn trigger_graph(&mut self, ui: &mut Ui, title: &str, accel_path: &str) {
        let pointer = "gamepad.navi.pointer";
        let deadzone = get_path(&self.model, &format!("{pointer}.trigger_deadzone"))
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let deadzone = deadzone.clamp(0.0, 0.9);
        let curve = get_path(&self.model, &format!("{pointer}.trigger_curve"))
            .and_then(Value::as_f64)
            .unwrap_or(1.35) as f32;
        let curve = curve.clamp(0.05, 5.0);
        let decel_floor = get_path(&self.model, &format!("{accel_path}.decel_floor"))
            .and_then(Value::as_f64)
            .unwrap_or(0.3) as f32;
        let decel_floor = decel_floor.clamp(0.01, 1.0);
        let boost_cap = get_path(&self.model, &format!("{accel_path}.boost_cap"))
            .and_then(Value::as_f64)
            .unwrap_or(2.5) as f32;
        let boost_cap = boost_cap.clamp(1.0, 10.0);
        let enabled = get_path(&self.model, &format!("{accel_path}.enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(true);

        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(11.5).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if !enabled {
                    ui.label(RichText::new("off").size(10.0).color(WARN));
                }
            });
        });
        let width = ui.available_width();
        let height = 178.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let plot = Rect::from_min_max(
            Pos2::new(rect.min.x + 34.0, rect.min.y + 6.0),
            Pos2::new(rect.max.x - 8.0, rect.max.y - 20.0),
        );
        let y_max = (boost_cap * 1.15).max(1.5);
        let to_pos = |pull: f32, multiplier: f32| {
            Pos2::new(
                plot.min.x + pull.clamp(0.0, 1.0) * plot.width(),
                plot.max.y - (multiplier / y_max).clamp(0.0, 1.0) * plot.height(),
            )
        };
        let to_multiplier = |y: f32| ((plot.max.y - y) / plot.height() * y_max).clamp(0.0, y_max);
        let painter = ui.painter().clone();
        painter.rect_filled(plot, 5.0, FIELD_BG);
        painter.rect_stroke(plot, 5.0, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
        for (value, label) in [
            (0.0, "0×".to_string()),
            (1.0, "1× base".to_string()),
            (y_max, format!("{y_max:.1}×")),
        ] {
            let y = to_pos(0.0, value).y;
            painter.line_segment(
                [Pos2::new(plot.min.x, y), Pos2::new(plot.max.x, y)],
                Stroke::new(
                    1.0_f32,
                    if value == 1.0 {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 90)
                    } else {
                        Color32::from_rgba_unmultiplied(255, 255, 255, 34)
                    },
                ),
            );
            painter.text(
                Pos2::new(plot.min.x - 5.0, y),
                Align2::RIGHT_CENTER,
                label,
                FontId::monospace(8.0),
                TEXT_DIM,
            );
        }
        let deadzone_x = to_pos(deadzone, 0.0).x;
        painter.line_segment(
            [
                Pos2::new(deadzone_x, plot.min.y),
                Pos2::new(deadzone_x, plot.max.y),
            ],
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 55)),
        );
        painter.text(
            Pos2::new(deadzone_x, plot.min.y + 2.0),
            Align2::CENTER_TOP,
            "deadzone",
            FontId::monospace(8.0),
            TEXT_DIM,
        );
        painter.text(
            Pos2::new(plot.min.x, plot.max.y + 4.0),
            Align2::LEFT_TOP,
            "0%",
            FontId::monospace(8.0),
            TEXT_DIM,
        );
        painter.text(
            Pos2::new(plot.center().x, plot.max.y + 4.0),
            Align2::CENTER_TOP,
            "trigger pull",
            FontId::monospace(8.0),
            TEXT_DIM,
        );
        painter.text(
            Pos2::new(plot.max.x, plot.max.y + 4.0),
            Align2::RIGHT_TOP,
            "100%",
            FontId::monospace(8.0),
            TEXT_DIM,
        );

        let lt_color = if enabled {
            WARN
        } else {
            Color32::from_rgba_unmultiplied(WARN.r(), WARN.g(), WARN.b(), 110)
        };
        let rt_color = if enabled {
            OK
        } else {
            Color32::from_rgba_unmultiplied(OK.r(), OK.g(), OK.b(), 110)
        };
        let lt_points = (0..=64)
            .map(|index| {
                let pull = index as f32 / 64.0;
                to_pos(pull, decel_floor.powf(trigger_shape(pull, deadzone, curve)))
            })
            .collect::<Vec<_>>();
        let rt_points = (0..=64)
            .map(|index| {
                let pull = index as f32 / 64.0;
                to_pos(pull, boost_cap.powf(trigger_shape(pull, deadzone, curve)))
            })
            .collect::<Vec<_>>();
        painter.add(egui::Shape::line(lt_points, Stroke::new(2.0_f32, lt_color)));
        painter.add(egui::Shape::line(rt_points, Stroke::new(2.0_f32, rt_color)));

        let lt_pos = to_pos(1.0, decel_floor);
        let rt_pos = to_pos(1.0, boost_cap);
        let shape_x = deadzone + (1.0 - deadzone) * 0.5;
        let shape_multiplier = boost_cap.powf(0.5_f32.powf(curve.max(0.05)));
        let shape_pos = to_pos(shape_x, shape_multiplier);
        let dz_pos = Pos2::new(deadzone_x, plot.max.y - 6.0);

        let lt_response = ui.interact(
            Rect::from_center_size(lt_pos, Vec2::splat(16.0)),
            ui.id().with((accel_path, "lt-floor")),
            Sense::click_and_drag(),
        );
        if lt_response.dragged()
            && let Some(position) = lt_response.interact_pointer_pos()
        {
            let value = to_multiplier(position.y).clamp(0.01, 1.0);
            set_json_path(
                &mut self.model,
                &format!("{accel_path}.decel_floor"),
                json!(value),
            );
        }
        let rt_response = ui.interact(
            Rect::from_center_size(rt_pos, Vec2::splat(16.0)),
            ui.id().with((accel_path, "rt-cap")),
            Sense::click_and_drag(),
        );
        if rt_response.dragged()
            && let Some(position) = rt_response.interact_pointer_pos()
        {
            let value = to_multiplier(position.y).clamp(1.0, 10.0);
            set_json_path(
                &mut self.model,
                &format!("{accel_path}.boost_cap"),
                json!(value),
            );
        }
        let dz_response = ui.interact(
            Rect::from_center_size(dz_pos, Vec2::new(16.0, 20.0)),
            ui.id().with((accel_path, "deadzone")),
            Sense::click_and_drag(),
        );
        if dz_response.dragged()
            && let Some(position) = dz_response.interact_pointer_pos()
        {
            let value = ((position.x - plot.min.x) / plot.width()).clamp(0.0, 0.9);
            set_json_path(
                &mut self.model,
                &format!("{pointer}.trigger_deadzone"),
                json!(value),
            );
        }
        if boost_cap > 1.01 {
            let shape_response = ui.interact(
                Rect::from_center_size(shape_pos, Vec2::splat(16.0)),
                ui.id().with((accel_path, "curve")),
                Sense::click_and_drag(),
            );
            if shape_response.dragged()
                && let Some(position) = shape_response.interact_pointer_pos()
            {
                let multiplier = to_multiplier(position.y).clamp(1.001, boost_cap - 0.001);
                let shaped = (multiplier.ln() / boost_cap.ln()).clamp(0.001, 0.999);
                let value = (shaped.ln() / 0.5_f32.ln()).clamp(0.05, 5.0);
                set_json_path(
                    &mut self.model,
                    &format!("{pointer}.trigger_curve"),
                    json!(value),
                );
            }
            paint_handle(
                &painter,
                shape_pos,
                TEXT_MUTED,
                shape_response.hovered() || shape_response.dragged(),
            );
            painter.text(
                Pos2::new(shape_pos.x + 8.0, shape_pos.y - 2.0),
                Align2::LEFT_BOTTOM,
                "shape",
                FontId::monospace(8.0),
                TEXT_DIM,
            );
            if shape_response.hovered() {
                shape_response
                    .clone()
                    .on_hover_cursor(egui::CursorIcon::ResizeVertical)
                    .on_hover_text(
                        "Response curve: 1.0 is linear; higher stays slower until deeper pull",
                    );
            }
        }

        paint_handle(
            &painter,
            lt_pos,
            lt_color,
            lt_response.hovered() || lt_response.dragged(),
        );
        paint_handle(
            &painter,
            rt_pos,
            rt_color,
            rt_response.hovered() || rt_response.dragged(),
        );
        painter.circle_filled(
            dz_pos,
            4.5,
            if dz_response.hovered() || dz_response.dragged() {
                Color32::WHITE
            } else {
                TEXT_MUTED
            },
        );
        painter.text(
            Pos2::new(dz_pos.x + 8.0, dz_pos.y - 2.0),
            Align2::LEFT_BOTTOM,
            "deadzone",
            FontId::monospace(8.0),
            TEXT_DIM,
        );
        if lt_response.hovered() {
            lt_response
                .on_hover_cursor(egui::CursorIcon::ResizeVertical)
                .on_hover_text("Full LT brake speed multiplier");
        }
        if rt_response.hovered() {
            rt_response
                .on_hover_cursor(egui::CursorIcon::ResizeVertical)
                .on_hover_text("Full RT gas speed multiplier");
        }
        if dz_response.hovered() {
            dz_response
                .on_hover_cursor(egui::CursorIcon::ResizeHorizontal)
                .on_hover_text("Initial trigger travel ignored");
        }

        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(format!(
                    "LT full brake {:.2}× ({:.0}% speed)",
                    decel_floor,
                    decel_floor * 100.0
                ))
                .monospace()
                .size(10.0)
                .color(WARN),
            );
            ui.label(
                RichText::new(format!(
                    "RT full gas {:.2}× ({:.0}% speed)",
                    boost_cap,
                    boost_cap * 100.0
                ))
                .monospace()
                .size(10.0)
                .color(OK),
            );
            ui.label(
                RichText::new(format!(
                    "deadzone {:.0}% · curve {:.2} (shared)",
                    deadzone * 100.0,
                    curve
                ))
                .monospace()
                .size(10.0)
                .color(TEXT_DIM),
            );
        });
    }

    fn pedal_simulation(&mut self, ui: &mut Ui) {
        let pointer = "gamepad.navi.pointer";
        let deadzone = get_path(&self.model, &format!("{pointer}.trigger_deadzone"))
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let deadzone = deadzone.clamp(0.0, 0.9);
        let curve = get_path(&self.model, &format!("{pointer}.trigger_curve"))
            .and_then(Value::as_f64)
            .unwrap_or(1.35) as f32;
        let curve = curve.clamp(0.05, 5.0);
        let pointer_floor = get_path(&self.model, &format!("{pointer}.accel.decel_floor"))
            .and_then(Value::as_f64)
            .unwrap_or(0.3) as f32;
        let pointer_floor = pointer_floor.clamp(0.01, 1.0);
        let pointer_cap = get_path(&self.model, &format!("{pointer}.accel.boost_cap"))
            .and_then(Value::as_f64)
            .unwrap_or(2.5) as f32;
        let pointer_cap = pointer_cap.clamp(1.0, 10.0);
        let pointer_enabled = get_path(&self.model, &format!("{pointer}.accel.enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let scroll_floor = get_path(&self.model, &format!("{pointer}.scroll_accel.decel_floor"))
            .and_then(Value::as_f64)
            .unwrap_or(0.3) as f32;
        let scroll_floor = scroll_floor.clamp(0.01, 1.0);
        let scroll_cap = get_path(&self.model, &format!("{pointer}.scroll_accel.boost_cap"))
            .and_then(Value::as_f64)
            .unwrap_or(2.5) as f32;
        let scroll_cap = scroll_cap.clamp(1.0, 10.0);
        let scroll_enabled = get_path(&self.model, &format!("{pointer}.scroll_accel.enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false);

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(
                    "Drag pedals to simulate pull; not live controller input. Uses the same trigger settings.",
                )
                .size(10.0)
                .color(TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Reset").clicked() {
                    self.sim_lt = 0.0;
                    self.sim_rt = 0.0;
                }
            });
        });
        ui.add_space(4.0);
        ui.columns(2, |cols| {
            pedal_control(
                &mut cols[0],
                "LT brake (decel)",
                &mut self.sim_lt,
                WARN,
                deadzone,
            );
            pedal_control(
                &mut cols[1],
                "RT gas (accel)",
                &mut self.sim_rt,
                OK,
                deadzone,
            );
        });
        ui.add_space(4.0);
        let lt_shape = trigger_shape(self.sim_lt, deadzone, curve);
        let rt_shape = trigger_shape(self.sim_rt, deadzone, curve);
        let pointer_multiplier = if pointer_enabled {
            trigger_multiplier(
                pointer_floor,
                pointer_cap,
                self.sim_lt,
                self.sim_rt,
                deadzone,
                curve,
            )
        } else {
            1.0
        };
        let scroll_multiplier = if scroll_enabled {
            trigger_multiplier(
                scroll_floor,
                scroll_cap,
                self.sim_lt,
                self.sim_rt,
                deadzone,
                curve,
            )
        } else {
            1.0
        };
        ui.label(
            RichText::new("Target speed vs baseline (no smoothing)")
                .size(9.5)
                .color(TEXT_DIM),
        );
        multiplier_row(ui, "Pointer", pointer_multiplier, pointer_enabled);
        multiplier_row(ui, "Scroll", scroll_multiplier, scroll_enabled);
        ui.label(
            RichText::new(format!(
                "After deadzone/curve: LT {:.0}% · RT {:.0}%",
                lt_shape * 100.0,
                rt_shape * 100.0
            ))
            .monospace()
            .size(9.5)
            .color(TEXT_DIM),
        );
    }

    fn accel_settings_group(&mut self, ui: &mut Ui, base: &str, title: &str) {
        mini_heading(ui, title);
        self.bool_field(ui, &format!("{base}.enabled"), "Enabled");
        self.f32_field(
            ui,
            &format!("{base}.decel_floor"),
            "Full brake ×",
            0.01,
            1.0,
            0.01,
        );
        self.f32_field(
            ui,
            &format!("{base}.boost_cap"),
            "Full gas ×",
            1.0,
            10.0,
            0.05,
        );
        self.f32_field(
            ui,
            &format!("{base}.smoothing_ms"),
            "Smoothing ms",
            0.0,
            500.0,
            1.0,
        );
        ui.label(
            RichText::new("1.0× = baseline; below 1 is slower, above 1 is faster.")
                .size(9.0)
                .color(TEXT_DIM),
        );
    }

    fn accel_fields(&mut self, ui: &mut Ui) {
        let pointer = "gamepad.navi.pointer";
        if ui.available_width() >= 660.0 {
            ui.columns(3, |cols| {
                mini_heading(&mut cols[0], "Trigger");
                self.f32_field(
                    &mut cols[0],
                    &format!("{pointer}.trigger_deadzone"),
                    "Pull deadzone",
                    0.0,
                    0.9,
                    0.005,
                );
                self.f32_field(
                    &mut cols[0],
                    &format!("{pointer}.trigger_curve"),
                    "Shape curve",
                    0.05,
                    5.0,
                    0.05,
                );
                self.f32_field(
                    &mut cols[0],
                    &format!("{pointer}.trigger_threshold"),
                    "Layer threshold",
                    0.0,
                    1.0,
                    0.01,
                );
                self.accel_settings_group(
                    &mut cols[1],
                    &format!("{pointer}.accel"),
                    "Pointer accel",
                );
                self.accel_settings_group(
                    &mut cols[2],
                    &format!("{pointer}.scroll_accel"),
                    "Scroll accel",
                );
            });
        } else {
            mini_heading(ui, "Trigger");
            self.f32_field(
                ui,
                &format!("{pointer}.trigger_deadzone"),
                "Pull deadzone",
                0.0,
                0.9,
                0.005,
            );
            self.f32_field(
                ui,
                &format!("{pointer}.trigger_curve"),
                "Shape curve",
                0.05,
                5.0,
                0.05,
            );
            self.f32_field(
                ui,
                &format!("{pointer}.trigger_threshold"),
                "Layer threshold",
                0.0,
                1.0,
                0.01,
            );
            ui.add_space(4.0);
            self.accel_settings_group(ui, &format!("{pointer}.accel"), "Pointer accel");
            ui.add_space(4.0);
            self.accel_settings_group(ui, &format!("{pointer}.scroll_accel"), "Scroll accel");
        }
    }

    fn xmb_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        section_card(ui, "Panel", |ui| {
            if ui.available_width() >= 480.0 {
                ui.columns(2, |cols| {
                    self.combo_field(
                        &mut cols[0],
                        "popup.outputs",
                        "Outputs",
                        &["focused", "all", "combined"],
                    );
                    self.combo_field(
                        &mut cols[0],
                        "popup.surface_mode",
                        "Surface",
                        &["fixed", "follow_active", "mirror"],
                    );
                    self.combo_field(
                        &mut cols[0],
                        "popup.position",
                        "Position",
                        &["cursor", "fixed", "center", "anchored"],
                    );
                    self.length_field(&mut cols[0], "popup.width", "Width");
                    self.length_field(&mut cols[0], "popup.height", "Height");
                    self.u64_field(
                        &mut cols[0],
                        "popup.follow_delay_ms",
                        "Follow delay ms",
                        0,
                        60_000,
                    );

                    self.length_field(&mut cols[1], "popup.fixed_x", "Fixed X");
                    self.length_field(&mut cols[1], "popup.fixed_y", "Fixed Y");
                    self.length_field(&mut cols[1], "popup.canvas_width", "Canvas width");
                    self.length_field(&mut cols[1], "popup.canvas_height", "Canvas height");
                    self.bool_field(&mut cols[1], "popup.show_labels", "Labels");
                    self.bool_field(&mut cols[1], "popup.show_titles", "Titles");
                });
            } else {
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
            }
        });
        section_card(ui, "Placements", |ui| {
            self.placements_ui(ui);
        });
        section_card(ui, "Behavior", |ui| {
            if ui.available_width() >= 480.0 {
                ui.columns(2, |cols| {
                    self.combo_field(
                        &mut cols[0],
                        "behavior.navigation",
                        "Navigation",
                        &["deferred", "live"],
                    );
                    self.bool_field(&mut cols[0], "behavior.move_popup", "Border drag");
                    self.f32_field(
                        &mut cols[0],
                        "behavior.popup_grab_region",
                        "Grab region",
                        0.0,
                        2048.0,
                        1.0,
                    );
                    self.f32_field(
                        &mut cols[0],
                        "behavior.drag_threshold",
                        "Drag threshold",
                        0.0,
                        2048.0,
                        0.5,
                    );
                    self.bool_field(
                        &mut cols[0],
                        "behavior.close_on_outside_click",
                        "Close outside",
                    );
                    self.bool_field(
                        &mut cols[0],
                        "behavior.replay_outside_click",
                        "Replay outside",
                    );
                    self.bool_field(
                        &mut cols[0],
                        "behavior.close_on_window_click",
                        "Close on window",
                    );

                    self.bool_field(
                        &mut cols[1],
                        "behavior.close_on_workspace_click",
                        "Close on workspace",
                    );
                    self.bool_field(&mut cols[1], "behavior.close_on_drop", "Close on drop");
                    self.bool_field(
                        &mut cols[1],
                        "behavior.close_on_external_focus_change",
                        "Close external focus",
                    );
                    self.bool_field(&mut cols[1], "behavior.close_on_escape", "Close on Escape");
                    self.bool_field(
                        &mut cols[1],
                        "behavior.empty_drop_floats",
                        "Empty drop floats",
                    );
                    self.bool_field(
                        &mut cols[1],
                        "behavior.floating_drop_tiles",
                        "Floating drop tiles",
                    );
                    self.bool_field(
                        &mut cols[1],
                        "behavior.focus_after_drop",
                        "Focus after drop",
                    );
                    self.bool_field(
                        &mut cols[1],
                        "behavior.snap_cursor_after_drop",
                        "Snap cursor after drop",
                    );
                });
            } else {
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
            }
        });
        section_card(ui, "Peek", |ui| {
            if ui.available_width() >= 480.0 {
                ui.columns(2, |cols| {
                    self.bool_field(&mut cols[0], "peek.enabled", "Enabled");
                    self.u64_field(&mut cols[0], "peek.duration_ms", "Duration ms", 0, 60_000);
                    self.bool_field(&mut cols[0], "peek.pointer_promote", "Pointer promote");
                    self.bool_field(&mut cols[1], "peek.move_popup", "Border move");
                    self.u64_field(&mut cols[1], "peek.arm_grace_ms", "Arm grace ms", 0, 60_000);
                    self.u64_field(&mut cols[1], "peek.demote_ms", "Demote ms", 0, 60_000);
                });
            } else {
                self.bool_field(ui, "peek.enabled", "Enabled");
                self.u64_field(ui, "peek.duration_ms", "Duration ms", 0, 60_000);
                self.bool_field(ui, "peek.pointer_promote", "Pointer promote");
                self.bool_field(ui, "peek.move_popup", "Border move");
                self.u64_field(ui, "peek.arm_grace_ms", "Arm grace ms", 0, 60_000);
                self.u64_field(ui, "peek.demote_ms", "Demote ms", 0, 60_000);
            }
        });
        section_card(ui, "Animation", |ui| {
            if ui.available_width() >= 480.0 {
                ui.columns(2, |cols| {
                    self.bool_field(&mut cols[0], "animation.enabled", "Enabled");
                    self.f32_field(
                        &mut cols[0],
                        "animation.panel_ms",
                        "Panel ms",
                        0.0,
                        60_000.0,
                        1.0,
                    );
                    self.f32_field(
                        &mut cols[0],
                        "animation.emphasis_ms",
                        "Emphasis ms",
                        0.0,
                        60_000.0,
                        1.0,
                    );
                    self.f32_field(
                        &mut cols[1],
                        "animation.scroll_ms",
                        "Scroll ms",
                        0.0,
                        60_000.0,
                        1.0,
                    );
                    self.f32_field(
                        &mut cols[1],
                        "animation.open_scale",
                        "Open scale",
                        0.0,
                        1.0,
                        0.01,
                    );
                });
            } else {
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
        });
    }

    fn style_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        section_card(ui, "Theme", |ui| {
            self.text_field(ui, "theme", "Theme file");
        });
        section_card(ui, "Colors", |ui| {
            if ui.available_width() >= 480.0 {
                let left = COLOR_FIELDS.len().div_ceil(2);
                ui.columns(2, |cols| {
                    for (index, (path, label)) in COLOR_FIELDS.iter().copied().enumerate() {
                        let column = if index < left { 0 } else { 1 };
                        self.color_field(&mut cols[column], path, label);
                    }
                });
            } else {
                for (path, label) in COLOR_FIELDS.iter().copied() {
                    self.color_field(ui, path, label);
                }
            }
            self.combo_field(
                ui,
                "style.urgent_indicator",
                "Urgent indicator",
                &["none", "ring", "glow", "badge"],
            );
        });
        section_card(ui, "Geometry", |ui| {
            if ui.available_width() >= 480.0 {
                let left = GEOMETRY_FIELDS.len().div_ceil(2);
                ui.columns(2, |cols| {
                    for (index, (path, label, min, max, step)) in
                        GEOMETRY_FIELDS.iter().copied().enumerate()
                    {
                        let column = if index < left { 0 } else { 1 };
                        self.f32_field(&mut cols[column], path, label, min, max, step);
                    }
                });
            } else {
                for (path, label, min, max, step) in GEOMETRY_FIELDS.iter().copied() {
                    self.f32_field(ui, path, label, min, max, step);
                }
            }
            self.u64_field(ui, "style.icon_size", "Icon size", 1, 1024);
            self.u64_field(ui, "style.icon_size_focused", "Focused icon size", 1, 1024);
        });
    }

    fn bars_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        section_card(ui, "Bars", |ui| {
            self.bool_field(ui, "bars_enabled", "Enabled");
            ui.add_space(4.0);
            let count = self.model["bars"].as_array().map_or(0, Vec::len);
            if count == 0 {
                ui.label(RichText::new("No bars").size(11.0).color(TEXT_MUTED));
            }
            let mut remove = None;
            for index in 0..count {
                let label = self.model["bars"][index]["output"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                let title = if label.is_empty() {
                    format!("Bar {}", index + 1)
                } else {
                    format!("Bar {} - {label}", index + 1)
                };
                let preview = self.model["bars"][index].clone();
                if index > 0 {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new(title).size(12.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Delete").clicked() {
                            remove = Some(index);
                        }
                    });
                });
                ui.add_space(4.0);
                let base = format!("bars.{index}");
                if ui.available_width() >= 480.0 {
                    ui.columns(2, |cols| {
                        self.text_field(&mut cols[0], &format!("{base}.output"), "Output");
                        self.combo_field(
                            &mut cols[0],
                            &format!("{base}.edge"),
                            "Edge",
                            &["top", "bottom", "left", "right"],
                        );
                        self.combo_field(
                            &mut cols[0],
                            &format!("{base}.align"),
                            "Align",
                            &["start", "center", "end"],
                        );
                        self.u64_field(&mut cols[0], &format!("{base}.width"), "Width", 1, 100_000);
                        self.u64_field(
                            &mut cols[0],
                            &format!("{base}.height"),
                            "Height",
                            1,
                            100_000,
                        );
                        self.i64_field(&mut cols[0], &format!("{base}.margin"), "Margin");
                        self.i64_field(&mut cols[0], &format!("{base}.offset"), "Offset");
                        self.bool_field(
                            &mut cols[0],
                            &format!("{base}.allow_column_join"),
                            "Column join",
                        );

                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.show_workspace_labels"),
                            "Workspace labels",
                        );
                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.hide_empty_workspaces"),
                            "Hide empty",
                        );
                        self.bool_field(&mut cols[1], &format!("{base}.show_capsule"), "Capsule");
                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.show_active_indicator"),
                            "Active indicator",
                        );
                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.scroll_to_switch"),
                            "Wheel switches",
                        );
                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.scroll_wrap"),
                            "Wheel wraps",
                        );
                        self.bool_field(
                            &mut cols[1],
                            &format!("{base}.scroll_reverse"),
                            "Reverse wheel",
                        );
                        self.u64_field(
                            &mut cols[1],
                            &format!("{base}.scroll_repeat_ms"),
                            "Wheel repeat ms",
                            0,
                            60_000,
                        );
                    });
                } else {
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
                }
                ui.add_space(6.0);
                self.bar_preview(ui, &preview, ui.available_width());
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
        });
    }

    fn preview_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("XMB panel").size(12.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} / {} / {}",
                            self.model["popup"]["outputs"].as_str().unwrap_or("focused"),
                            self.model["popup"]["position"].as_str().unwrap_or("cursor"),
                            self.model["popup"]["surface_mode"]
                                .as_str()
                                .unwrap_or("fixed")
                        ))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                    );
                });
            });
            ui.add_space(6.0);
            self.xmb_preview(ui, ui.available_width());
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(format!(
                        "labels {}",
                        if self.model["popup"]["show_labels"]
                            .as_bool()
                            .unwrap_or(false)
                        {
                            "on"
                        } else {
                            "off"
                        }
                    ))
                    .monospace()
                    .size(9.5)
                    .color(TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "titles {}",
                        if self.model["popup"]["show_titles"]
                            .as_bool()
                            .unwrap_or(false)
                        {
                            "on"
                        } else {
                            "off"
                        }
                    ))
                    .monospace()
                    .size(9.5)
                    .color(TEXT_DIM),
                );
                ui.label(
                    RichText::new(format!(
                        "anim {}",
                        self.model["animation"]["panel_ms"].as_f64().unwrap_or(0.0)
                    ))
                    .monospace()
                    .size(9.5)
                    .color(TEXT_DIM),
                );
            });
        });
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Bars").size(12.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} bars", array_len(&self.model["bars"])))
                            .monospace()
                            .size(9.5)
                            .color(TEXT_DIM),
                    );
                });
            });
            ui.add_space(4.0);
            if !self.model["bars_enabled"].as_bool().unwrap_or(false) {
                ui.label(RichText::new("Bars disabled").size(11.0).color(TEXT_MUTED));
            } else {
                let bars = self.model["bars"].as_array().cloned().unwrap_or_default();
                if bars.is_empty() {
                    ui.label(RichText::new("No bars").size(11.0).color(TEXT_MUTED));
                }
                for (index, bar) in bars.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("Bar {}", index + 1)).size(11.0));
                        ui.label(
                            RichText::new(bar["output"].as_str().unwrap_or(""))
                                .monospace()
                                .size(9.5)
                                .color(TEXT_DIM),
                        );
                    });
                    self.bar_preview(ui, bar, ui.available_width());
                    ui.add_space(6.0);
                }
            }
        });
    }

    fn save_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        card(ui, |ui| {
            ui.label(RichText::new("File").size(12.0).strong());
            ui.add_space(2.0);
            ui.label(RichText::new(&self.config_path).monospace().size(11.0));
            ui.horizontal(|ui| {
                let dot = if !self.error.is_empty() {
                    ERROR
                } else if self.dirty() {
                    WARN
                } else {
                    OK
                };
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.5, dot);
                ui.label(
                    RichText::new(if self.dirty() {
                        "Unsaved changes"
                    } else {
                        "Clean"
                    })
                    .size(11.0),
                );
                ui.label(
                    RichText::new(format!("sha {}", self.short_sha()))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                );
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Validate").clicked() {
                    self.validate();
                }
                let dirty = self.dirty();
                ui.add_enabled_ui(dirty, |ui| {
                    if ui.button("Save").clicked() {
                        self.confirm = Some(Confirm::Save);
                    }
                    if ui.button("Save & Restart").clicked() {
                        self.confirm = Some(Confirm::Restart);
                    }
                    if ui.button("Revert").clicked() {
                        self.revert();
                    }
                });
            });
        });

        let patch = self.patch();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Changes").size(12.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} set / {} remove",
                            patch.set.len(),
                            patch.remove.len()
                        ))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                    );
                });
            });
            ui.add_space(4.0);
            if patch.is_empty() {
                ui.label(RichText::new("No changes").size(11.0).color(TEXT_MUTED));
            } else {
                ScrollArea::vertical()
                    .id_salt("patch-paths")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for path in patch.set.keys() {
                            ui.label(
                                RichText::new(format!("set    {path}"))
                                    .monospace()
                                    .size(10.0),
                            );
                        }
                        for path in &patch.remove {
                            ui.label(
                                RichText::new(format!("remove {path}"))
                                    .monospace()
                                    .size(10.0),
                            );
                        }
                    });
            }
        });

        if !self.candidate.is_empty() {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Validated TOML").size(12.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(self.candidate.clone());
                        }
                    });
                });
                ui.add_space(4.0);
                let mut candidate = self.candidate.clone();
                ui.add(
                    egui::TextEdit::multiline(&mut candidate)
                        .code_editor()
                        .desired_rows(22)
                        .interactive(false),
                );
            });
        }
    }

    fn text_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = field_row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut value).desired_width(input_width(ui)))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, Value::String(value));
        }
    }

    fn bool_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let changed = field_row(ui, label, |ui| ui.checkbox(&mut value, "").changed());
        if changed {
            set_json_path(&mut self.model, path, Value::Bool(value));
        }
    }

    fn f32_field(&mut self, ui: &mut Ui, path: &str, label: &str, min: f64, max: f64, speed: f64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let changed = field_row(ui, label, |ui| {
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

    fn u64_field(&mut self, ui: &mut Ui, path: &str, label: &str, min: u64, max: u64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let changed = field_row(ui, label, |ui| {
            ui.add(egui::DragValue::new(&mut value).speed(1.0).range(min..=max))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
    }

    fn i64_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let changed = field_row(ui, label, |ui| {
            ui.add(egui::DragValue::new(&mut value).speed(1.0))
                .changed()
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
    }

    fn combo_field(&mut self, ui: &mut Ui, path: &str, label: &str, options: &[&str]) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old = value.clone();
        let changed = field_row(ui, label, |ui| {
            let mut changed = false;
            ComboBox::from_id_salt(path)
                .width(input_width(ui).clamp(90.0, 280.0))
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

    fn list_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
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
        let changed = field_row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(input_width(ui)))
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

    fn length_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let value = get_path(&self.model, path)
            .map(|value| match value {
                Value::String(value) => value.clone(),
                Value::Number(value) => value.to_string(),
                _ => String::new(),
            })
            .unwrap_or_default();
        let mut text = value;
        let changed = field_row(ui, label, |ui| {
            ui.add(egui::TextEdit::singleline(&mut text).desired_width(input_width(ui).min(170.0)))
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

    fn color_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = field_row(ui, label, |ui| {
            let changed = ui
                .add(
                    egui::TextEdit::singleline(&mut value)
                        .desired_width(input_width(ui).min(150.0)),
                )
                .changed();
            ui.label(RichText::new("■").color(parse_color(&value)));
            changed
        });
        if changed {
            set_json_path(&mut self.model, path, Value::String(value));
        }
    }

    fn bindings_ui(&mut self, ui: &mut Ui, path: &str, actions: &[&str], title: &str) {
        let entries = get_path(&self.model, path)
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(code, action)| (code.clone(), action.as_str().unwrap_or("").to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(11.5).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{} entries", entries.len()))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                );
            });
        });
        ui.add_space(3.0);
        if entries.is_empty() {
            ui.label(RichText::new("No bindings").size(11.0).color(TEXT_MUTED));
        } else if ui.available_width() >= 640.0 && entries.len() > 1 {
            let left = entries.len().div_ceil(2);
            ui.columns(2, |cols| {
                for (index, (code, action)) in entries.iter().enumerate() {
                    let column = if index < left { 0 } else { 1 };
                    self.binding_row(&mut cols[column], path, actions, code, action);
                }
            });
        } else {
            for (code, action) in &entries {
                self.binding_row(ui, path, actions, code, action);
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

    fn binding_row(&mut self, ui: &mut Ui, path: &str, actions: &[&str], code: &str, action: &str) {
        let mut next_code = code.to_string();
        let mut next_action = action.to_string();
        let mut remove = false;
        let changed = ui
            .horizontal(|ui| {
                let code_changed = ui
                    .add(
                        egui::TextEdit::singleline(&mut next_code)
                            .desired_width(120.0)
                            .font(egui::TextStyle::Monospace),
                    )
                    .changed();
                let mut action_changed = false;
                ComboBox::from_id_salt(("binding", path, code))
                    .width((ui.available_width() - 34.0).clamp(100.0, 320.0))
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
            })
            .inner;
        if remove {
            remove_map_key(&mut self.model, path, code);
        } else if next_code != code {
            remove_map_key(&mut self.model, path, code);
            set_map_value(&mut self.model, path, &next_code, &next_action);
        } else if changed || next_action != action {
            set_map_value(&mut self.model, path, code, &next_action);
        }
    }

    fn layers_ui(&mut self, ui: &mut Ui) {
        let names = self.model["gamepad"]["navi"]["layers"]
            .as_object()
            .map(|layers| layers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if names.is_empty() {
            ui.label(RichText::new("No layers").size(11.0).color(TEXT_MUTED));
        }
        let mut remove_layer = None;
        let mut first = true;
        for name in names {
            let path = format!("gamepad.navi.layers.{name}");
            if !first {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
            }
            first = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new(&name).monospace().size(11.5).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Delete").clicked() {
                        remove_layer = Some(name.clone());
                    }
                });
            });
            ui.add_space(4.0);
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
                        .add(
                            egui::TextEdit::singleline(&mut axis)
                                .desired_width(90.0)
                                .font(egui::TextStyle::Monospace),
                        )
                        .changed()
                    {
                        set_json_path(
                            &mut self.model,
                            &format!("{axis_path}.axis"),
                            Value::String(axis),
                        );
                    }
                    let mut threshold = get_path(&self.model, &format!("{axis_path}.threshold"))
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
                    self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"].as_array_mut()
            {
                axes.remove(index);
            }
            if ui.button("Add hold axis").clicked()
                && let Some(axes) =
                    self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"].as_array_mut()
            {
                axes.push(json!({"axis": "ABS_Z", "threshold": 0.35}));
            }
            self.bindings_ui(ui, &format!("{path}.bindings"), ACTIONS, "Bindings");
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

    fn placements_ui(&mut self, ui: &mut Ui) {
        let count = self.model["popup"]["placements"]
            .as_array()
            .map_or(0, Vec::len);
        if count == 0 {
            ui.label(RichText::new("No placements").size(11.0).color(TEXT_MUTED));
        }
        let mut remove = None;
        let mut first = true;
        for index in 0..count {
            let output = self.model["popup"]["placements"][index]["output"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let title = if output.is_empty() {
                format!("Placement {}", index + 1)
            } else {
                format!("Placement {} - {output}", index + 1)
            };
            if !first {
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
            }
            first = false;
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(11.5).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.small_button("Delete").clicked() {
                        remove = Some(index);
                    }
                });
            });
            ui.add_space(4.0);
            let base = format!("popup.placements.{index}");
            if ui.available_width() >= 480.0 {
                ui.columns(2, |cols| {
                    self.text_field(&mut cols[0], &format!("{base}.output"), "Output");
                    self.combo_field(
                        &mut cols[0],
                        &format!("{base}.x"),
                        "X",
                        &["left", "center", "right"],
                    );
                    self.combo_field(
                        &mut cols[0],
                        &format!("{base}.y"),
                        "Y",
                        &["top", "center", "bottom"],
                    );
                    self.length_field(&mut cols[1], &format!("{base}.margin"), "Margin");
                });
            } else {
                self.text_field(ui, &format!("{base}.output"), "Output");
                self.combo_field(ui, &format!("{base}.x"), "X", &["left", "center", "right"]);
                self.combo_field(ui, &format!("{base}.y"), "Y", &["top", "center", "bottom"]);
                self.length_field(ui, &format!("{base}.margin"), "Margin");
            }
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

    fn xmb_preview(&self, ui: &mut Ui, max_width: f32) {
        let requested_width =
            resolve_length(self.model["popup"]["width"].clone(), 920.0).clamp(240.0, 920.0);
        let width = requested_width.min(max_width.max(180.0)).floor();
        let scale = (width / 920.0).clamp(0.28, 1.0);
        let requested_height =
            resolve_length(self.model["popup"]["height"].clone(), 250.0).clamp(120.0, 420.0);
        let height = (requested_height * (width / requested_width))
            .clamp(110.0, requested_height)
            .floor();
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
        let radius = style["panel_radius"].as_f64().unwrap_or(20.0) as f32 * scale;
        let border_width = (style["border_width"].as_f64().unwrap_or(1.0) as f32 * scale).max(0.5);
        let shadow_offset =
            (style["shadow_offset"].as_f64().unwrap_or(6.0) as f32 * scale).max(0.0);
        let shadow_spread =
            (style["shadow_spread"].as_f64().unwrap_or(4.0) as f32 * scale).max(0.0);
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
            Stroke::new(border_width, border),
            StrokeKind::Inside,
        );
        let padding =
            (style["panel_padding"].as_f64().unwrap_or(14.0) as f32 * scale).clamp(4.0, 40.0);
        let inner = rect.shrink(padding);
        let canvas_gap =
            (style["canvas_gap"].as_f64().unwrap_or(12.0) as f32 * scale).clamp(2.0, 30.0);
        let canvas_width = ((inner.width() - canvas_gap) / 2.0).max(20.0);
        let footer = 30.0 * scale;
        let canvas_height = (inner.height() - footer).max(30.0);
        let font = FontId::monospace((8.0 * scale).max(6.0));
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
                (5.0 * scale).max(2.0),
                Stroke::new(border_width.min(1.0), border),
                StrokeKind::Inside,
            );
            let configured_icon = if index == 0 {
                style["icon_size_focused"].as_f64().unwrap_or(40.0) as f32
            } else {
                style["icon_size"].as_f64().unwrap_or(28.0) as f32
            };
            let icon_gap = (8.0 * scale).max(2.0);
            let left = (16.0 * scale).max(4.0);
            let icon_limit = ((canvas.width() - left - icon_gap * 2.0) / 3.0)
                .min(canvas.height() - 4.0)
                .max(4.0);
            let icon_size = (configured_icon * scale).clamp(4.0, icon_limit);
            for item in 0..3 {
                let icon = Rect::from_center_size(
                    Pos2::new(
                        canvas.min.x
                            + left
                            + icon_size / 2.0
                            + item as f32 * (icon_size + icon_gap),
                        canvas.min.y + canvas.height() / 2.0,
                    ),
                    Vec2::splat(icon_size),
                );
                let selected = index == 0 && item == 1;
                painter.rect_filled(
                    icon,
                    4.0 * scale,
                    if selected { surface_hover } else { surface },
                );
                painter.rect_stroke(
                    icon,
                    4.0 * scale,
                    Stroke::new(
                        border_width.min(1.0),
                        if selected { selection } else { border },
                    ),
                    StrokeKind::Inside,
                );
            }
            if self.model["popup"]["show_labels"]
                .as_bool()
                .unwrap_or(false)
            {
                painter.text(
                    Pos2::new(canvas.min.x + 4.0 * scale, canvas.min.y + 3.0 * scale),
                    Align2::LEFT_TOP,
                    format!("WS {}", index + 1),
                    font.clone(),
                    text,
                );
            }
        }
        painter.text(
            Pos2::new(inner.min.x, inner.max.y - 10.0 * scale),
            Align2::LEFT_BOTTOM,
            format!(
                "{} / {}",
                self.model["popup"]["position"].as_str().unwrap_or(""),
                self.model["popup"]["surface_mode"].as_str().unwrap_or("")
            ),
            font,
            text,
        );
        painter.circle_filled(
            Pos2::new(inner.max.x - 8.0 * scale, inner.max.y - 8.0 * scale),
            (3.0 * scale).max(1.5),
            accent,
        );
    }

    fn bar_preview(&self, ui: &mut Ui, bar: &Value, max_width: f32) {
        let style = &self.model["style"];
        let width = bar["width"]
            .as_f64()
            .unwrap_or(420.0)
            .clamp(200.0, 640.0)
            .min(max_width.max(160.0) as f64) as f32;
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
                    let label = if matches!(confirm, Confirm::Restart) {
                        "Save & Restart"
                    } else {
                        "Save"
                    };
                    if ui.button(label).clicked() {
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
        self.ensure_textures(ctx);
        self.command_bar(ctx);
        self.nav_rail(ctx);
        if self.show_preview {
            self.preview_pane(ctx);
        }
        self.content(ctx);
        self.confirm_modal(ctx);
    }
}

fn configure(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.window_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.extreme_bg_color = FIELD_BG;
    visuals.faint_bg_color = CARD;
    visuals.text_edit_bg_color = Some(FIELD_BG);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(116, 162, 255, 80);
    visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(32, 37, 45);
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(32, 37, 45);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(42, 49, 60);
    visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(42, 49, 60);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    visuals.widgets.active.bg_fill = Color32::from_rgb(46, 56, 72);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(46, 56, 72);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, Color32::WHITE);
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 4.0);
    style.spacing.interact_size.y = 22.0;
    ctx.set_style(style);
}

fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let response = egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        });
    ui.add_space(10.0);
    response.inner
}

fn section_card(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).size(12.5).strong());
            ui.add_space(4.0);
            ui.separator();
            ui.add_space(6.0);
            add(ui);
        });
    ui.add_space(8.0);
}

fn load_texture(ctx: &egui::Context, template: Template) -> Option<TextureHandle> {
    let image = image::load_from_memory(template.image()).ok()?;
    let rgba = image.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    let pixels = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
    Some(ctx.load_texture(template.texture_name(), pixels, TextureOptions::LINEAR))
}

fn field_row(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui) -> bool) -> bool {
    let label_width = (ui.available_width() * 0.42).clamp(74.0, 150.0);
    ui.horizontal(|ui| {
        ui.add_sized(
            Vec2::new(label_width, 22.0),
            egui::Label::new(RichText::new(label).size(12.0).color(TEXT_MUTED)).truncate(),
        );
        add(ui)
    })
    .inner
}

fn field_label(ui: &mut Ui, label: &str) {
    let width = (ui.available_width() * 0.32).clamp(56.0, 100.0);
    ui.add_sized(
        Vec2::new(width, 22.0),
        egui::Label::new(RichText::new(label).size(12.0).color(TEXT_MUTED)).truncate(),
    );
}

fn input_width(ui: &Ui) -> f32 {
    (ui.available_width() - 2.0).clamp(64.0, 360.0)
}

fn mini_heading(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(10.5).strong().color(TEXT_DIM));
    ui.add_space(2.0);
}

fn paint_handle(painter: &egui::Painter, center: Pos2, color: Color32, active: bool) {
    let radius = if active { 6.5 } else { 5.0 };
    painter.circle_filled(center, radius, color);
    painter.circle_stroke(
        center,
        radius + 1.0,
        Stroke::new(1.0_f32, Color32::from_rgb(240, 244, 250)),
    );
}

fn multiplier_row(ui: &mut Ui, label: &str, value: f32, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{label} speed"))
                .size(11.0)
                .color(TEXT_MUTED),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("{value:.2}× baseline"))
                    .monospace()
                    .size(13.0)
                    .color(if enabled { TEXT } else { TEXT_DIM }),
            );
            if !enabled {
                ui.label(RichText::new("off").size(10.0).color(WARN));
            }
        });
    });
}

fn pedal_control(ui: &mut Ui, label: &str, value: &mut f32, color: Color32, deadzone: f32) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 136.0), Sense::click_and_drag());
    let top = rect.min.y + 26.0;
    let bottom = rect.max.y - 20.0;
    if (response.clicked() || response.dragged())
        && let Some(position) = response.interact_pointer_pos()
    {
        *value = ((bottom - position.y) / (bottom - top).max(1.0)).clamp(0.0, 1.0);
    }
    let painter = ui.painter().clone();
    painter.text(
        Pos2::new(rect.min.x, rect.min.y + 2.0),
        Align2::LEFT_TOP,
        label,
        FontId::proportional(11.0),
        TEXT,
    );
    let center = rect.center().x;
    let track = Rect::from_min_max(
        Pos2::new(center - 21.0, top),
        Pos2::new(center + 21.0, bottom),
    );
    painter.rect_filled(track, 6.0, FIELD_BG);
    painter.rect_stroke(track, 6.0, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
    let travel = bottom - top;
    let fill_top = bottom - travel * *value;
    if *value > 0.0 {
        let fill = Rect::from_min_max(
            Pos2::new(track.min.x + 3.0, fill_top),
            Pos2::new(track.max.x - 3.0, bottom - 3.0),
        );
        painter.rect_filled(
            fill,
            4.0,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 180),
        );
    }
    for step in 0..=4 {
        let y = bottom - travel * step as f32 / 4.0;
        painter.line_segment(
            [
                Pos2::new(track.max.x - 6.0, y),
                Pos2::new(track.max.x - 2.0, y),
            ],
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 45)),
        );
    }
    let deadzone_y = bottom - travel * deadzone.clamp(0.0, 1.0);
    painter.line_segment(
        [
            Pos2::new(track.min.x - 7.0, deadzone_y),
            Pos2::new(track.max.x + 7.0, deadzone_y),
        ],
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 70)),
    );
    painter.text(
        Pos2::new(track.max.x + 9.0, deadzone_y),
        Align2::LEFT_CENTER,
        "dz",
        FontId::monospace(8.0),
        TEXT_DIM,
    );
    let plate = Rect::from_center_size(Pos2::new(center, fill_top), Vec2::new(62.0, 10.0));
    painter.rect_filled(plate, 3.0, color);
    painter.rect_stroke(
        plate,
        3.0,
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 120)),
        StrokeKind::Inside,
    );
    painter.circle_filled(Pos2::new(center, bottom + 5.0), 3.0, TEXT_DIM);
    painter.text(
        Pos2::new(center, rect.max.y - 2.0),
        Align2::CENTER_BOTTOM,
        format!("{:.0}%", *value * 100.0),
        FontId::monospace(10.0),
        TEXT_MUTED,
    );
    if response.hovered() {
        response.on_hover_cursor(egui::CursorIcon::ResizeVertical);
    }
}

fn nav_item(ui: &mut Ui, selected: bool, label: &str, hint: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 40.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 7.0, NAV_SELECTED);
        let bar = Rect::from_min_max(
            Pos2::new(rect.min.x, rect.min.y + 9.0),
            Pos2::new(rect.min.x + 3.0, rect.max.y - 9.0),
        );
        ui.painter().rect_filled(bar, 1.5, ACCENT);
    } else if response.hovered() {
        ui.painter()
            .rect_filled(rect, 7.0, Color32::from_rgba_unmultiplied(255, 255, 255, 8));
    }
    ui.painter().text(
        Pos2::new(rect.min.x + 13.0, rect.center().y - 6.0),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.5),
        if selected {
            Color32::from_rgb(236, 241, 250)
        } else {
            Color32::from_rgb(190, 198, 211)
        },
    );
    ui.painter().text(
        Pos2::new(rect.min.x + 13.0, rect.center().y + 8.0),
        Align2::LEFT_CENTER,
        hint,
        FontId::proportional(10.0),
        if selected {
            Color32::from_rgb(150, 166, 192)
        } else {
            TEXT_DIM
        },
    );
    if response.hovered() {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
}

fn metric_row(ui: &mut Ui, label: &str, value: impl ToString) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(11.0).color(TEXT_MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(value.to_string())
                    .monospace()
                    .size(11.0)
                    .color(TEXT),
            );
        });
    });
}

fn map_len(value: &Value) -> usize {
    value.as_object().map_or(0, |map| map.len())
}

fn array_len(value: &Value) -> usize {
    value.as_array().map_or(0, Vec::len)
}

fn status_color(status: &str) -> Color32 {
    match status {
        "Valid" | "Saved" | "Saved and restarted" => OK,
        "Invalid" | "Not saved" | "Saved; restart failed" => ERROR,
        _ => TEXT_MUTED,
    }
}

fn control_hint(template: Template, control: &str) -> Option<&'static str> {
    match control {
        "L STICK" | "R STICK" | "L3" | "R3" => {
            Some("Pointer and scroll settings are under Pointer.")
        }
        "LT" | "RT" | "L2" | "R2" => {
            Some("Curves and pedals are under Trigger response and Trigger settings.")
        }
        "D-PAD" => Some("D-pad actions resolve through the active layer or XMB."),
        "Touchpad" if template == Template::Ds4 => Some("Touchpad has no gamepad binding."),
        _ => None,
    }
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

fn trigger_shape(value: f32, deadzone: f32, curve: f32) -> f32 {
    let value = ((value.clamp(0.0, 1.0) - deadzone) / (1.0 - deadzone).max(0.001)).clamp(0.0, 1.0);
    value.powf(curve.max(0.05))
}

fn trigger_multiplier(
    decel_floor: f32,
    boost_cap: f32,
    lt: f32,
    rt: f32,
    deadzone: f32,
    curve: f32,
) -> f32 {
    decel_floor.powf(trigger_shape(lt, deadzone, curve))
        * boost_cap.powf(trigger_shape(rt, deadzone, curve))
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
            .with_inner_size([1440.0, 920.0])
            .with_min_inner_size([1180.0, 720.0]),
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
    fn trigger_shape_matches_daemon_math() {
        assert!(trigger_shape(0.0, 0.08, 1.0).abs() < 1e-6);
        assert!(trigger_shape(0.08, 0.08, 1.0).abs() < 1e-6);
        assert!((trigger_shape(1.0, 0.08, 1.0) - 1.0).abs() < 1e-6);
        assert!((trigger_shape(0.5, 0.0, 1.0) - 0.5).abs() < 1e-6);
        assert!(trigger_shape(0.5, 0.0, 2.0) < 0.5);
        assert!((trigger_shape(0.5, 0.0, 0.0) - trigger_shape(0.5, 0.0, 0.05)).abs() < 1e-6);
    }

    #[test]
    fn trigger_multiplier_matches_daemon_targets() {
        let multiplier = |lt, rt| trigger_multiplier(0.3, 2.5, lt, rt, 0.08, 1.0);
        assert!((multiplier(0.0, 0.0) - 1.0).abs() < 1e-5);
        assert!((multiplier(1.0, 0.0) - 0.3).abs() < 1e-5);
        assert!((multiplier(0.0, 1.0) - 2.5).abs() < 1e-5);
        assert!((multiplier(1.0, 1.0) - 0.75).abs() < 1e-5);
        assert!((multiplier(0.08, 0.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn ds4_hotspots_map_linux_button_codes() {
        let hotspots = Template::Ds4.hotspots();
        for (control, code) in [
            ("Cross", "BTN_SOUTH"),
            ("Circle", "BTN_EAST"),
            ("Square", "BTN_WEST"),
            ("Triangle", "BTN_NORTH"),
            ("L1", "BTN_TL"),
            ("R1", "BTN_TR"),
            ("L2", "BTN_TL2"),
            ("R2", "BTN_TR2"),
            ("Share", "BTN_SELECT"),
            ("Options", "BTN_START"),
            ("PS", "BTN_MODE"),
            ("L3", "BTN_THUMBL"),
            ("R3", "BTN_THUMBR"),
        ] {
            let hotspot = hotspots
                .iter()
                .find(|hotspot| hotspot.control == control)
                .unwrap_or_else(|| panic!("missing {control}"));
            assert_eq!(hotspot.code, Some(code), "{control}");
        }
        assert!(
            hotspots
                .iter()
                .any(|hotspot| hotspot.control == "Touchpad" && hotspot.code.is_none())
        );
    }

    #[test]
    fn template_switch_maps_equivalent_controls() {
        assert_eq!(Template::Xbox.default_control(), "A");
        assert_eq!(Template::Ds4.default_control(), "Cross");
        assert_eq!(Template::Xbox.counterpart("L2"), Some("LT"));
        assert_eq!(Template::Xbox.counterpart("L3"), Some("L STICK"));
        assert_eq!(Template::Ds4.counterpart("A"), Some("Cross"));
        assert_eq!(Template::Ds4.counterpart("L STICK"), Some("L3"));
        assert_eq!(Template::Ds4.counterpart("D-PAD"), Some("D-PAD"));
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
