//! Native editor for niri-xmb TOML.
//!
//! Left navigation selects a section, the command bar loads, validates and
//! saves, and a persistent live dock shows the popup render, controller state,
//! and the Niri keybind map. The editor loads the real schema through
//! `niri-xmb config-json`, writes only changed paths through `toml_edit`,
//! validates with `config-check`, and saves atomically. It does not talk to the
//! daemon except through the existing `restart` command after an explicit Save
//! & Restart.

mod i18n;
mod keymap;

use anyhow::{Context, Result};
use eframe::egui;
use egui::{
    Align, Align2, Color32, ColorImage, ComboBox, CursorIcon, FontId, Layout, Pos2, Rect, RichText,
    ScrollArea, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use niri_xmb::config::Config;
use niri_xmb::preview::{
    PopupPreview, PreviewFrame, PreviewSelection, PreviewTarget, snapshot_from_json,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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
/// Standard gamepad buttons offered by the binding dropdowns, with the usual
/// Xbox and DualShock labels. Stored values stay evdev names so the TOML also
/// matches the daemon's presets.
const BUTTONS: &[(&str, &str)] = &[
    ("BTN_DPAD_UP", "D-pad up"),
    ("BTN_DPAD_DOWN", "D-pad down"),
    ("BTN_DPAD_LEFT", "D-pad left"),
    ("BTN_DPAD_RIGHT", "D-pad right"),
    ("BTN_SOUTH", "A / Cross"),
    ("BTN_EAST", "B / Circle"),
    ("BTN_WEST", "X / Square"),
    ("BTN_NORTH", "Y / Triangle"),
    ("BTN_TL", "LB / L1"),
    ("BTN_TR", "RB / R1"),
    ("BTN_TL2", "LT / L2"),
    ("BTN_TR2", "RT / R2"),
    ("BTN_SELECT", "Back / Share"),
    ("BTN_START", "Start / Options"),
    ("BTN_MODE", "Guide / PS"),
    ("BTN_THUMBL", "L3"),
    ("BTN_THUMBR", "R3"),
    ("BTN_TRIGGER_HAPPY1", "Paddle 1"),
    ("BTN_TRIGGER_HAPPY2", "Paddle 2"),
    ("BTN_TRIGGER_HAPPY3", "Paddle 3"),
    ("BTN_TRIGGER_HAPPY4", "Paddle 4"),
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
        x: 0.248,
        y: 0.139,
        w: 0.067,
        h: 0.081,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "RT",
        code: None,
        x: 0.752,
        y: 0.139,
        w: 0.067,
        h: 0.081,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "LB",
        code: Some("BTN_TL"),
        x: 0.222,
        y: 0.238,
        w: 0.164,
        h: 0.067,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "RB",
        code: Some("BTN_TR"),
        x: 0.773,
        y: 0.238,
        w: 0.164,
        h: 0.067,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "L STICK",
        code: None,
        x: 0.223,
        y: 0.52,
        w: 0.13,
        h: 0.134,
        circle: true,
        label: true,
    },
    Hotspot {
        control: "D-PAD",
        code: None,
        x: 0.356,
        y: 0.705,
        w: 0.145,
        h: 0.125,
        circle: false,
        label: true,
    },
    Hotspot {
        control: "R STICK",
        code: None,
        x: 0.627,
        y: 0.715,
        w: 0.127,
        h: 0.128,
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
        x: 0.847,
        y: 0.489,
        w: 0.072,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "A",
        code: Some("BTN_SOUTH"),
        x: 0.776,
        y: 0.59,
        w: 0.075,
        h: 0.075,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "SELECT",
        code: Some("BTN_SELECT"),
        x: 0.399,
        y: 0.497,
        w: 0.047,
        h: 0.047,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "START",
        code: Some("BTN_START"),
        x: 0.598,
        y: 0.497,
        w: 0.047,
        h: 0.047,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "GUIDE",
        code: Some("BTN_MODE"),
        x: 0.499,
        y: 0.489,
        w: 0.095,
        h: 0.095,
        circle: true,
        label: false,
    },
];

const DS4_HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "Touchpad",
        code: None,
        x: 0.499,
        y: 0.344,
        w: 0.324,
        h: 0.223,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "L2",
        code: Some("BTN_TL2"),
        x: 0.196,
        y: 0.060,
        w: 0.13,
        h: 0.072,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "R2",
        code: Some("BTN_TR2"),
        x: 0.803,
        y: 0.060,
        w: 0.13,
        h: 0.072,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "L1",
        code: Some("BTN_TL"),
        x: 0.196,
        y: 0.127,
        w: 0.137,
        h: 0.061,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "R1",
        code: Some("BTN_TR"),
        x: 0.803,
        y: 0.127,
        w: 0.137,
        h: 0.061,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "Triangle",
        code: Some("BTN_NORTH"),
        x: 0.803,
        y: 0.392,
        w: 0.066,
        h: 0.066,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Circle",
        code: Some("BTN_EAST"),
        x: 0.874,
        y: 0.508,
        w: 0.066,
        h: 0.066,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Cross",
        code: Some("BTN_SOUTH"),
        x: 0.803,
        y: 0.621,
        w: 0.066,
        h: 0.066,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Square",
        code: Some("BTN_WEST"),
        x: 0.732,
        y: 0.508,
        w: 0.066,
        h: 0.066,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "D-PAD",
        code: None,
        x: 0.196,
        y: 0.500,
        w: 0.188,
        h: 0.188,
        circle: false,
        label: true,
    },
    Hotspot {
        control: "L3",
        code: Some("BTN_THUMBL"),
        x: 0.349,
        y: 0.773,
        w: 0.106,
        h: 0.106,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "R3",
        code: Some("BTN_THUMBR"),
        x: 0.654,
        y: 0.773,
        w: 0.106,
        h: 0.106,
        circle: true,
        label: false,
    },
    Hotspot {
        control: "Share",
        code: Some("BTN_SELECT"),
        x: 0.312,
        y: 0.341,
        w: 0.033,
        h: 0.059,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "Options",
        code: Some("BTN_START"),
        x: 0.700,
        y: 0.341,
        w: 0.033,
        h: 0.059,
        circle: false,
        label: false,
    },
    Hotspot {
        control: "PS",
        code: Some("BTN_MODE"),
        x: 0.502,
        y: 0.692,
        w: 0.055,
        h: 0.055,
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
    Trigger,
    Popup,
    Appearance,
}

impl Section {
    const ALL: [(Section, &'static str); 4] = [
        (Self::Controller, "Controller"),
        (Self::Trigger, "Trigger Lab"),
        (Self::Popup, "Popup"),
        (Self::Appearance, "Appearance"),
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Trigger => "Trigger Lab",
            Self::Popup => "Popup",
            Self::Appearance => "Appearance",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Controller => "Device, pointer, bindings, layers",
            Self::Trigger => "Response curve and speed tuning",
            Self::Popup => "Panel, behavior, peek, animation",
            Self::Appearance => "Theme, colors, bars",
        }
    }
}

/// Tabs in the persistent live dock.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DockTab {
    Popup,
    Controller,
    Keyboard,
}

impl DockTab {
    const ALL: [DockTab; 3] = [Self::Popup, Self::Controller, Self::Keyboard];

    fn label(self) -> &'static str {
        match self {
            Self::Popup => "Popup",
            Self::Controller => "Controller",
            Self::Keyboard => "Keyboard",
        }
    }

    /// The tab that best matches an editor section for auto-follow.
    fn for_section(section: Section) -> Self {
        match section {
            Section::Controller | Section::Trigger => Self::Controller,
            Section::Popup | Section::Appearance => Self::Popup,
        }
    }
}

#[derive(Clone, Copy)]
enum Confirm {
    Save,
    Restart,
}

#[derive(Clone, Default)]
struct LiveState {
    status: Value,
    snapshot: Value,
    error: String,
}

fn spawn_live(
    niri_xmb: PathBuf,
    live: Arc<Mutex<LiveState>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("niri-xmb-studio-live".into())
        .spawn(move || {
            // The daemon mirrors its snapshot into the runtime directory, so
            // status plus a file read is enough after the first iteration.
            let mut mirror: Option<PathBuf> = None;
            while !stop.load(Ordering::Relaxed) {
                let status = run_niri(&niri_xmb, &["status"])
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                if let Some(path) = status
                    .as_ref()
                    .and_then(|status| status["snapshot_mirror"].as_str())
                {
                    mirror = Some(PathBuf::from(path));
                }
                let snapshot = mirror
                    .as_ref()
                    .and_then(|path| fs::read_to_string(path).ok())
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .or_else(|| {
                        run_niri(&niri_xmb, &["snapshot"])
                            .ok()
                            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    });
                if let Ok(mut state) = live.lock() {
                    if let Some(status) = status {
                        state.status = status;
                        state.error.clear();
                    }
                    if let Some(snapshot) = snapshot {
                        state.snapshot = snapshot;
                    }
                    if state.status.is_null() && state.snapshot.is_null() {
                        state.error = "daemon status unavailable".to_string();
                    }
                }
                thread::sleep(Duration::from_millis(400));
            }
        })
        .expect("spawn live studio poller")
}

impl Drop for StudioApp {
    fn drop(&mut self) {
        self.live_stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.live_thread.take() {
            let _ = handle.join();
        }
    }
}

struct StudioApp {
    niri_xmb: PathBuf,
    config_path: String,
    model: Value,
    loaded: Value,
    sha256: String,
    section: Section,
    show_changes: bool,
    show_candidate: bool,
    template: Template,
    selected: &'static str,
    context: String,
    status: String,
    error: String,
    candidate: String,
    confirm: Option<Confirm>,
    sim_lt: f32,
    sim_rt: f32,
    sim_last_left: bool,
    sim_first_left: bool,
    selected_point: Option<usize>,
    live: Arc<Mutex<LiveState>>,
    live_stop: Arc<AtomicBool>,
    live_thread: Option<JoinHandle<()>>,
    xbox_texture: Option<TextureHandle>,
    ds4_texture: Option<TextureHandle>,
    preview: Option<PopupPreview>,
    preview_texture: Option<TextureHandle>,
    preview_frame: Option<PreviewFrame>,
    preview_key: String,
    preview_hover: Option<PreviewTarget>,
    preview_selection: Option<PreviewSelection>,
    preview_selection_at: Instant,
    dock_tab: DockTab,
    dock_collapsed: bool,
    dock_height: f32,
    dock_follow: bool,
    dock_section: Section,
    keymap: keymap::NiriKeymap,
    keymap_at: Instant,
    keymap_search: String,
    keymap_selected: Option<String>,
}

impl StudioApp {
    fn new(cc: &eframe::CreationContext<'_>, config: PathBuf, niri_xmb: PathBuf) -> Self {
        configure(&cc.egui_ctx);
        let mut app = Self {
            niri_xmb: niri_xmb.clone(),
            config_path: config.display().to_string(),
            model: Value::Null,
            loaded: Value::Null,
            sha256: String::new(),
            section: Section::Controller,
            show_changes: false,
            show_candidate: false,
            template: Template::Xbox,
            selected: Template::Xbox.default_control(),
            context: "base".to_string(),
            status: String::new(),
            error: String::new(),
            candidate: String::new(),
            confirm: None,
            sim_lt: 0.0,
            sim_rt: 0.0,
            sim_last_left: true,
            sim_first_left: true,
            selected_point: None,
            live: Arc::new(Mutex::new(LiveState::default())),
            live_stop: Arc::new(AtomicBool::new(false)),
            live_thread: None,
            xbox_texture: None,
            ds4_texture: None,
            preview: None,
            preview_texture: None,
            preview_frame: None,
            preview_key: String::new(),
            preview_hover: None,
            preview_selection: None,
            preview_selection_at: Instant::now(),
            dock_tab: DockTab::Popup,
            dock_collapsed: false,
            dock_height: 360.0,
            dock_follow: true,
            dock_section: Section::Controller,
            keymap: keymap::NiriKeymap::load_default(),
            keymap_at: Instant::now(),
            keymap_search: String::new(),
            keymap_selected: None,
        };
        app.live_thread = Some(spawn_live(
            niri_xmb,
            Arc::clone(&app.live),
            Arc::clone(&app.live_stop),
        ));
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
            self.show_changes = false;
            self.show_candidate = false;
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
        self.show_changes = false;
        self.show_candidate = false;
    }

    fn revert(&mut self) {
        self.model = self.loaded.clone();
        self.error.clear();
        self.candidate.clear();
        self.status = "Reverted".to_string();
        self.show_changes = false;
        self.show_candidate = false;
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
        if let Some(error) = self.trigger_points_error() {
            self.error = error;
            self.status = "Invalid".to_string();
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
        if let Some(error) = self.trigger_points_error() {
            self.error = error;
            self.status = "Invalid".to_string();
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
        self.show_changes = false;
        self.show_candidate = false;
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

    fn request_save(&mut self, restart_after: bool) {
        if let Some(error) = self.trigger_points_error() {
            self.error = error;
            self.status = "Invalid".to_string();
            return;
        }
        self.confirm = Some(if restart_after {
            Confirm::Restart
        } else {
            Confirm::Save
        });
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
                let wide = ui.available_width() >= 760.0;
                if wide {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("niri-xmb").size(15.0).strong());
                        ui.label(RichText::new("Config").size(10.5).color(TEXT_DIM));
                        ui.add_space(8.0);
                        let path_width = (ui.available_width() - 390.0).clamp(140.0, 620.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config_path)
                                .desired_width(path_width)
                                .hint_text("~/.config/niri-xmb/config.toml"),
                        );
                        if tt(ui.button("Load"), "ui.action.load").clicked() {
                            self.load();
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            self.daemon_chip(ui);
                        });
                    });
                } else {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("niri-xmb").size(15.0).strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            self.daemon_chip(ui);
                        });
                    });
                    ui.horizontal(|ui| {
                        let path_width = (ui.available_width() - 58.0).max(80.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.config_path)
                                .desired_width(path_width)
                                .hint_text("~/.config/niri-xmb/config.toml"),
                        );
                        if tt(ui.button("Load"), "ui.action.load").clicked() {
                            self.load();
                        }
                    });
                }
            });
    }

    fn daemon_chip(&self, ui: &mut Ui) {
        let live = self
            .live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default();
        let gamepad = &live.status["gamepad"];
        let devices = gamepad["devices"].as_u64().unwrap_or(0);
        let (dot, text) = if live.status.is_null() {
            (TEXT_DIM, "daemon offline".to_string())
        } else if devices == 0 {
            (WARN, "no controller".to_string())
        } else {
            (
                OK,
                format!(
                    "{} · {} · {}",
                    gamepad["mode"].as_str().unwrap_or("-"),
                    gamepad["preset"].as_str().unwrap_or("-"),
                    devices
                ),
            )
        };
        egui::Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(999.0)
            .inner_margin(egui::Margin::symmetric(9, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, dot);
                    ui.label(RichText::new(text).monospace().size(10.0).color(TEXT_MUTED));
                });
            });
    }

    fn action_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("action-bar")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 7))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                if ui.available_width() >= 900.0 {
                    ui.horizontal(|ui| {
                        self.action_status(ui);
                        ui.separator();
                        self.action_buttons(ui);
                    });
                } else {
                    self.action_status(ui);
                    ui.horizontal_wrapped(|ui| {
                        self.action_buttons(ui);
                    });
                }
                if self.show_changes {
                    ui.separator();
                    self.changes_drawer(ui);
                }
            });
    }

    fn action_status(&self, ui: &mut Ui) {
        let patch = self.patch();
        let dot = if !self.error.is_empty() {
            ERROR
        } else if !patch.is_empty() {
            WARN
        } else {
            OK
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, dot);
        ui.label(
            RichText::new(if patch.is_empty() {
                "No changes".to_string()
            } else {
                format!("{} changes", patch.set.len() + patch.remove.len())
            })
            .size(11.0)
            .color(TEXT),
        );
        ui.label(
            RichText::new(format!("sha {}", self.short_sha()))
                .monospace()
                .size(9.5)
                .color(TEXT_DIM),
        );
        if !self.status.is_empty() {
            ui.label(
                RichText::new(&self.status)
                    .size(10.5)
                    .color(if self.error.is_empty() {
                        status_color(&self.status)
                    } else {
                        ERROR
                    }),
            );
        }
    }

    fn action_buttons(&mut self, ui: &mut Ui) {
        let dirty = self.dirty();
        if tt(
            ui.selectable_label(self.show_changes, "Changes"),
            "ui.action.changes",
        )
        .clicked()
        {
            self.show_changes = !self.show_changes;
        }
        if tt(ui.button("Preview"), "ui.action.preview").clicked() {
            self.dock_tab = DockTab::Popup;
            self.dock_collapsed = false;
            self.dock_follow = true;
        }
        if tt(
            ui.add_enabled(self.model != Value::Null, egui::Button::new("Validate")),
            "ui.action.validate",
        )
        .clicked()
        {
            self.validate();
        }
        if tt(
            ui.add_enabled(dirty, egui::Button::new("Save")),
            "ui.action.save",
        )
        .clicked()
        {
            self.request_save(false);
        }
        if tt(
            ui.add_enabled(dirty, egui::Button::new("Save & Restart")),
            "ui.action.save_restart",
        )
        .clicked()
        {
            self.request_save(true);
        }
        if tt(
            ui.add_enabled(dirty, egui::Button::new("Revert")),
            "ui.action.revert",
        )
        .clicked()
        {
            self.revert();
        }
    }

    fn changes_drawer(&mut self, ui: &mut Ui) {
        let patch = self.patch();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Pending changes").size(11.0).strong());
            if !self.candidate.is_empty() {
                if ui
                    .selectable_label(self.show_candidate, "View TOML")
                    .clicked()
                {
                    self.show_candidate = !self.show_candidate;
                }
                if tt(ui.small_button("Copy TOML"), "ui.action.copy_toml").clicked() {
                    ui.ctx().copy_text(self.candidate.clone());
                }
            }
        });
        if patch.is_empty() {
            ui.label(RichText::new("No changes").size(10.5).color(TEXT_MUTED));
        } else {
            ScrollArea::vertical()
                .id_salt("patch-paths")
                .max_height(100.0)
                .show(ui, |ui| {
                    for path in patch.set.keys() {
                        ui.label(
                            RichText::new(format!("set    {path}"))
                                .monospace()
                                .size(9.5),
                        );
                    }
                    for path in &patch.remove {
                        ui.label(
                            RichText::new(format!("remove {path}"))
                                .monospace()
                                .size(9.5),
                        );
                    }
                });
        }
        if self.show_candidate && !self.candidate.is_empty() {
            let mut candidate = self.candidate.clone();
            ui.add(
                egui::TextEdit::multiline(&mut candidate)
                    .code_editor()
                    .desired_rows(10)
                    .interactive(false),
            );
        }
    }

    fn nav_rail(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav-rail")
            .exact_width(184.0)
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
                for (section, label) in Section::ALL {
                    if nav_item(ui, self.section == section, label).clicked() {
                        self.section = section;
                    }
                    ui.add_space(2.0);
                }
            });
    }

    fn nav_strip(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("nav-strip")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(10, 5))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (section, label) in Section::ALL {
                        if ui
                            .selectable_label(self.section == section, label)
                            .clicked()
                        {
                            self.section = section;
                        }
                    }
                });
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
                        if tt(ui.small_button("Dismiss"), "ui.action.dismiss").clicked() {
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
                            Section::Trigger => self.trigger_section(ui),
                            Section::Popup => self.popup_section(ui),
                            Section::Appearance => self.appearance_section(ui),
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
        });
        ui.add_space(10.0);
    }

    fn controller_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        self.detected_controller_card(ui);
        self.controller_map_card(ui);
        if ui.available_width() >= 900.0 {
            ui.columns(2, |cols| {
                section_card(&mut cols[0], "Device", |ui| self.device_fields(ui));
                section_card(&mut cols[1], "Pointer", |ui| self.pointer_fields(ui));
            });
        } else {
            section_card(ui, "Device", |ui| self.device_fields(ui));
            section_card(ui, "Pointer", |ui| self.pointer_fields(ui));
        }
        section_card(ui, "Layers", |ui| self.layers_ui(ui));
    }

    fn detected_controller_card(&self, ui: &mut Ui) {
        let live = self
            .live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default();
        let status = &live.status;
        let gamepad = &status["gamepad"];
        let connected = gamepad["connected"].as_array().cloned().unwrap_or_default();
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Detected controller").size(12.0).strong());
                let dot = if !connected.is_empty() { OK } else { WARN };
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 3.0, dot);
                ui.label(
                    RichText::new(if connected.is_empty() {
                        "not detected"
                    } else {
                        "connected"
                    })
                    .size(10.0)
                    .color(dot),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(error) = status["error"].as_str() {
                        ui.label(RichText::new(error).size(9.5).color(ERROR));
                    } else if !live.error.is_empty() {
                        ui.label(RichText::new(&live.error).size(9.5).color(ERROR));
                    }
                });
            });
            if connected.is_empty() {
                ui.label(
                    RichText::new("No controller connected")
                        .size(11.0)
                        .color(TEXT_MUTED),
                );
            } else {
                for device in connected {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(device["name"].as_str().unwrap_or("controller"))
                                .size(11.0)
                                .color(TEXT),
                        );
                        ui.label(
                            RichText::new(device["path"].as_str().unwrap_or(""))
                                .monospace()
                                .size(9.0)
                                .color(TEXT_DIM),
                        );
                    });
                }
            }
        });
        ui.add_space(8.0);
    }

    fn trigger_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        if ui.available_width() >= 900.0 {
            ui.columns(2, |cols| {
                section_card(&mut cols[0], "Response", |ui| self.trigger_response(ui));
                section_card(&mut cols[1], "Tune", |ui| {
                    self.pedal_simulation(ui);
                    ui.add_space(8.0);
                    ui.separator();
                    ui.add_space(6.0);
                    self.accel_fields(ui);
                });
            });
        } else {
            section_card(ui, "Response", |ui| self.trigger_response(ui));
            section_card(ui, "Tune", |ui| {
                self.pedal_simulation(ui);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                self.accel_fields(ui);
            });
        }
    }

    fn controller_map_card(&mut self, ui: &mut Ui) {
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Controller map").size(12.0).strong());
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
                    ui.label(
                        RichText::new("bindings follow the selected control")
                            .size(9.5)
                            .color(TEXT_DIM),
                    );
                });
            });
            ui.add_space(6.0);
            let mut hotspot = None;
            let mut row = None;
            let width = ui.available_width();
            if width >= 900.0 {
                let map_width = (width * 0.46).clamp(400.0, 560.0);
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(map_width);
                        hotspot = self.controller_map_body(ui);
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(4.0);
                        self.inspector_body(ui);
                    });
                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        row = self.merged_bindings(ui);
                    });
                });
            } else {
                hotspot = self.controller_map_body(ui);
                ui.separator();
                self.inspector_body(ui);
                ui.separator();
                self.merged_bindings(ui);
            }
            if let (Some(from), Some(to)) = (hotspot, row) {
                paint_binding_link(ui, from, to);
            }
        });
    }

    fn controller_map_body(&mut self, ui: &mut Ui) -> Option<Rect> {
        let template = self.template;
        let width = ui.available_width().min(560.0);
        let height = (width * 0.56).clamp(210.0, 330.0);
        let (box_rect, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        if let Some(backdrop) = template.backdrop() {
            ui.painter().rect_filled(box_rect, 10.0, backdrop);
            ui.painter().rect_stroke(
                box_rect,
                10.0,
                Stroke::new(1.0_f32, BORDER),
                StrokeKind::Inside,
            );
        }
        let image_size = if width * template.aspect() <= height {
            Vec2::new(width, width * template.aspect())
        } else {
            Vec2::new(height / template.aspect(), height)
        };
        let rect = Rect::from_center_size(box_rect.center(), image_size);
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
        let mut selected_rect = None;
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
                selected_rect = Some(hit);
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
                let pos = Pos2::new(center.x, hit.max.y + 3.0);
                let font = FontId::monospace(label_size);
                ui.painter().text(
                    pos + Vec2::new(0.0, 1.0),
                    Align2::CENTER_TOP,
                    hotspot.control,
                    font.clone(),
                    Color32::from_rgba_unmultiplied(12, 14, 18, 200),
                );
                ui.painter().text(
                    pos,
                    Align2::CENTER_TOP,
                    hotspot.control,
                    font,
                    Color32::from_gray(166),
                );
            }
            if response.clicked() {
                self.selected = hotspot.control;
            }
        }
        selected_rect
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

    fn selected_code(&self) -> Option<&'static str> {
        self.template
            .hotspots()
            .iter()
            .find(|hotspot| hotspot.control == self.selected)
            .and_then(|hotspot| hotspot.code)
    }

    fn inspector_body(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Selected")
                .size(11.0)
                .strong()
                .color(TEXT_MUTED),
        );
        ui.add_space(2.0);
        let code = self.selected_code();
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
                self.button_list_field(&mut cols[0], "gamepad.open_combo", "Open combo");

                self.combo_field(
                    &mut cols[1],
                    "gamepad.mode.default",
                    "Default mode",
                    &["game", "navi"],
                );
                self.button_list_field(&mut cols[1], "gamepad.mode.toggle", "Mode toggle");
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
            self.button_list_field(ui, "gamepad.open_combo", "Open combo");
            self.combo_field(
                ui,
                "gamepad.mode.default",
                "Default mode",
                &["game", "navi"],
            );
            self.button_list_field(ui, "gamepad.mode.toggle", "Mode toggle");
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
                    "Full-stick speed",
                    0.0,
                    100000.0,
                    1.0,
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
                "Full-stick speed",
                0.0,
                100000.0,
                1.0,
            );
        }
    }

    fn trigger_points_error(&self) -> Option<String> {
        let deadzone = get_path(&self.model, "gamepad.navi.pointer.trigger_deadzone")
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let points = self.trigger_points();
        if points.len() > 5 {
            return Some("At most five points".to_string());
        }
        let mut previous_pull = deadzone;
        let mut previous_output = 0.0_f32;
        for (index, point) in points.iter().enumerate() {
            if !point.pull.is_finite() || !point.output.is_finite() {
                return Some(format!("Point {} has a non-finite value", index + 1));
            }
            if point.pull <= deadzone || point.pull >= 1.0 {
                return Some(format!(
                    "Point {} pull must be inside the trigger",
                    index + 1
                ));
            }
            if point.pull - previous_pull < 0.10 {
                return Some(format!(
                    "Point {} must be at least 10% after the previous point",
                    index + 1
                ));
            }
            if point.output < previous_output || !(0.0..=1.0).contains(&point.output) {
                return Some(format!(
                    "Point {} shape must stay between 0% and 100% and not decrease",
                    index + 1
                ));
            }
            previous_pull = point.pull;
            previous_output = point.output;
        }
        None
    }

    fn trigger_points(&self) -> Vec<CurvePoint> {
        get_path(&self.model, "gamepad.navi.pointer.trigger_points")
            .and_then(Value::as_array)
            .map(|points| {
                points
                    .iter()
                    .filter_map(|point| {
                        Some(CurvePoint {
                            pull: point.get("pull")?.as_f64()? as f32,
                            output: point.get("output")?.as_f64()? as f32,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn set_trigger_points(&mut self, points: &[CurvePoint]) {
        let mut points = points.to_vec();
        points.sort_by(|a, b| a.pull.total_cmp(&b.pull));
        let values = points
            .iter()
            .map(|point| json!({"pull": point.pull, "output": point.output}))
            .collect::<Vec<_>>();
        set_json_path(
            &mut self.model,
            "gamepad.navi.pointer.trigger_points",
            Value::Array(values),
        );
        if self
            .selected_point
            .is_some_and(|index| index >= points.len())
        {
            self.selected_point = None;
        }
    }

    fn add_trigger_point(&mut self) {
        let points = self.trigger_points();
        if points.len() >= 5 {
            return;
        }
        let deadzone = get_path(&self.model, "gamepad.navi.pointer.trigger_deadzone")
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let curve = get_path(&self.model, "gamepad.navi.pointer.trigger_curve")
            .and_then(Value::as_f64)
            .unwrap_or(1.35) as f32;
        let Some(pull) = [0.50_f32, 0.60, 0.40, 0.70, 0.30, 0.80]
            .into_iter()
            .find(|pull| {
                *pull > deadzone + 0.10
                    && points
                        .iter()
                        .all(|point| (point.pull - *pull).abs() >= 0.10)
            })
        else {
            return;
        };
        let output = trigger_shape(pull, deadzone, curve, &points);
        let mut next = points;
        next.push(CurvePoint { pull, output });
        next.sort_by(|a, b| a.pull.total_cmp(&b.pull));
        self.selected_point = next.iter().position(|point| point.pull == pull);
        self.set_trigger_points(&next);
    }

    fn trigger_deadzone_field(&mut self, ui: &mut Ui) {
        let points = self.trigger_points();
        let max = points
            .first()
            .map(|point| (point.pull - 0.10).max(0.0))
            .unwrap_or(0.9)
            .min(0.9);
        let mut value = get_path(&self.model, "gamepad.navi.pointer.trigger_deadzone")
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let changed = field_row_path(
            ui,
            Some("gamepad.navi.pointer.trigger_deadzone"),
            "Pull deadzone",
            |ui| {
                ui.add(
                    egui::DragValue::new(&mut value)
                        .speed(0.005)
                        .range(0.0..=max),
                )
                .changed()
            },
        );
        if changed {
            set_json_path(
                &mut self.model,
                "gamepad.navi.pointer.trigger_deadzone",
                json!(value),
            );
        }
        if value > max {
            ui.colored_label(ERROR, "deadzone is too close to the first point");
        }
    }

    fn trigger_presets(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Shape").size(10.0).color(TEXT_DIM));
            if ui.small_button("+ Point").clicked() {
                self.add_trigger_point();
            }
            if ui.small_button("Linear").clicked() {
                set_json_path(
                    &mut self.model,
                    "gamepad.navi.pointer.trigger_curve",
                    json!(1.0),
                );
                self.set_trigger_points(&[]);
            }
            for (label, points) in [
                (
                    "Soft start",
                    vec![
                        CurvePoint {
                            pull: 0.35,
                            output: 0.10,
                        },
                        CurvePoint {
                            pull: 0.65,
                            output: 0.40,
                        },
                    ],
                ),
                (
                    "Precise",
                    vec![
                        CurvePoint {
                            pull: 0.35,
                            output: 0.08,
                        },
                        CurvePoint {
                            pull: 0.65,
                            output: 0.30,
                        },
                        CurvePoint {
                            pull: 0.85,
                            output: 0.65,
                        },
                    ],
                ),
                (
                    "Snappy",
                    vec![
                        CurvePoint {
                            pull: 0.35,
                            output: 0.05,
                        },
                        CurvePoint {
                            pull: 0.65,
                            output: 0.55,
                        },
                        CurvePoint {
                            pull: 0.85,
                            output: 0.90,
                        },
                    ],
                ),
            ] {
                if ui.small_button(label).clicked() {
                    self.set_trigger_points(&points);
                }
            }
            if ui.small_button("Convert curve").clicked() {
                let deadzone = get_path(&self.model, "gamepad.navi.pointer.trigger_deadzone")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.08) as f32;
                let curve = get_path(&self.model, "gamepad.navi.pointer.trigger_curve")
                    .and_then(Value::as_f64)
                    .unwrap_or(1.35) as f32;
                let points = [0.25_f32, 0.50, 0.75]
                    .into_iter()
                    .map(|pull| CurvePoint {
                        pull,
                        output: trigger_shape(pull, deadzone, curve, &[]),
                    })
                    .collect::<Vec<_>>();
                self.set_trigger_points(&points);
            }
            if ui.small_button("Clear").clicked() {
                self.set_trigger_points(&[]);
            }
        });
    }

    fn point_inspector(&mut self, ui: &mut Ui) {
        let points = self.trigger_points();
        let Some(index) = self.selected_point.filter(|index| *index < points.len()) else {
            return;
        };
        let start = ui.cursor().min;
        let invalid = self.trigger_points_error();
        let point = points[index];
        let deadzone = get_path(&self.model, "gamepad.navi.pointer.trigger_deadzone")
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let curve = get_path(&self.model, "gamepad.navi.pointer.trigger_curve")
            .and_then(Value::as_f64)
            .unwrap_or(1.35) as f32;
        let shape = trigger_shape(point.pull, deadzone, curve, &points);
        let pointer_floor = get_path(&self.model, "gamepad.navi.pointer.accel.decel_floor")
            .and_then(Value::as_f64)
            .unwrap_or(0.3) as f32;
        let pointer_cap = get_path(&self.model, "gamepad.navi.pointer.accel.boost_cap")
            .and_then(Value::as_f64)
            .unwrap_or(2.5) as f32;
        let scroll_floor = get_path(&self.model, "gamepad.navi.pointer.scroll_accel.decel_floor")
            .and_then(Value::as_f64)
            .unwrap_or(0.35) as f32;
        let scroll_cap = get_path(&self.model, "gamepad.navi.pointer.scroll_accel.boost_cap")
            .and_then(Value::as_f64)
            .unwrap_or(3.0) as f32;
        ui.separator();
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("Point {}", index + 1)).strong());
            let mut pull = point.pull;
            let mut output = point.output;
            let pull_changed = ui
                .add(
                    egui::DragValue::new(&mut pull)
                        .speed(0.005)
                        .range(deadzone + 0.001..=0.999)
                        .suffix(" pull"),
                )
                .changed();
            let output_changed = ui
                .add(
                    egui::DragValue::new(&mut output)
                        .speed(0.005)
                        .range(0.0..=1.0)
                        .suffix(" shape"),
                )
                .changed();
            if ui.small_button("Delete point").clicked() {
                let mut next = points.clone();
                next.remove(index);
                self.set_trigger_points(&next);
                self.selected_point = None;
                return;
            }
            if pull_changed || output_changed {
                let mut next = points.clone();
                let previous_pull = index
                    .checked_sub(1)
                    .map(|previous| next[previous].pull)
                    .unwrap_or(deadzone + 0.001);
                let next_pull = next.get(index + 1).map(|next| next.pull).unwrap_or(0.999);
                let previous_output = index
                    .checked_sub(1)
                    .map(|previous| next[previous].output)
                    .unwrap_or(0.0);
                let next_output = next.get(index + 1).map(|next| next.output).unwrap_or(1.0);
                let min_pull = previous_pull + 0.10;
                let max_pull = next_pull - 0.10;
                if min_pull <= max_pull {
                    next[index].pull = pull.clamp(min_pull, max_pull);
                }
                next[index].output = output.clamp(previous_output, next_output);
                self.set_trigger_points(&next);
            }
        });
        ui.label(
            RichText::new(format!(
                "Pull {:.0}% → shape {:.0}%",
                point.pull * 100.0,
                shape * 100.0
            ))
            .monospace()
            .size(10.0)
            .color(TEXT_MUTED),
        );
        ui.label(
            RichText::new(format!(
                "Pointer: LT {:.2}× ({:.0}%) · RT {:.2}× ({:.0}%)",
                pointer_floor.powf(shape),
                pointer_floor.powf(shape) * 100.0,
                pointer_cap.powf(shape),
                pointer_cap.powf(shape) * 100.0,
            ))
            .monospace()
            .size(10.0)
            .color(TEXT_MUTED),
        );
        ui.label(
            RichText::new(format!(
                "Scroll: LT {:.2}× ({:.0}%) · RT {:.2}× ({:.0}%)",
                scroll_floor.powf(shape),
                scroll_floor.powf(shape) * 100.0,
                scroll_cap.powf(shape),
                scroll_cap.powf(shape) * 100.0,
            ))
            .monospace()
            .size(10.0)
            .color(TEXT_MUTED),
        );
        if let Some(error) = invalid {
            let rect = Rect::from_min_max(start, ui.min_rect().max).expand(3.0);
            ui.painter()
                .rect_stroke(rect, 5.0, Stroke::new(1.5_f32, ERROR), StrokeKind::Inside);
            ui.colored_label(ERROR, error);
        }
    }

    fn trigger_response(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("LT brake · RT gas · 1.0× baseline")
                .size(10.0)
                .color(TEXT_DIM),
        );
        let socd = get_path(&self.model, "gamepad.navi.pointer.speed_socd")
            .and_then(Value::as_str)
            .unwrap_or("multiply");
        let socd_label = match socd {
            "last" => "last pressed wins",
            "first" => "first held wins",
            "cancel" => "both cancel",
            "prio_decel" => "brake wins",
            "prio_accel" => "gas wins",
            _ => "brake × gas",
        };
        ui.label(
            RichText::new(format!("Speed conflict: {socd_label}"))
                .size(10.0)
                .color(TEXT_DIM),
        );
        self.trigger_presets(ui);
        ui.add_space(6.0);
        let pointer_cap = get_path(&self.model, "gamepad.navi.pointer.accel.boost_cap")
            .and_then(Value::as_f64)
            .unwrap_or(2.5) as f32;
        let scroll_cap = get_path(&self.model, "gamepad.navi.pointer.scroll_accel.boost_cap")
            .and_then(Value::as_f64)
            .unwrap_or(3.0) as f32;
        let shared_y_max = (pointer_cap.max(scroll_cap) * 1.15).max(1.5);
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                self.trigger_graph(
                    &mut cols[0],
                    "Pointer",
                    "gamepad.navi.pointer.accel",
                    shared_y_max,
                );
                self.trigger_graph(
                    &mut cols[1],
                    "Scroll",
                    "gamepad.navi.pointer.scroll_accel",
                    shared_y_max,
                );
            });
        } else {
            self.trigger_graph(ui, "Pointer", "gamepad.navi.pointer.accel", shared_y_max);
            ui.add_space(6.0);
            self.trigger_graph(
                ui,
                "Scroll",
                "gamepad.navi.pointer.scroll_accel",
                shared_y_max,
            );
        }
        if let Some(error) = self.trigger_points_error() {
            ui.colored_label(ERROR, error);
        }
        self.point_inspector(ui);
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "Double-click the graph to add a point; drag points; right-click to delete.",
            )
            .size(9.5)
            .color(TEXT_DIM),
        );
    }

    fn trigger_graph(&mut self, ui: &mut Ui, title: &str, accel_path: &str, y_max: f32) {
        let pointer = "gamepad.navi.pointer";
        let deadzone = get_path(&self.model, &format!("{pointer}.trigger_deadzone"))
            .and_then(Value::as_f64)
            .unwrap_or(0.08) as f32;
        let deadzone = deadzone.clamp(0.0, 0.9);
        let curve = get_path(&self.model, &format!("{pointer}.trigger_curve"))
            .and_then(Value::as_f64)
            .unwrap_or(1.35) as f32;
        let curve = curve.clamp(0.05, 5.0);
        let points = self.trigger_points();
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
                to_pos(
                    pull,
                    decel_floor.powf(trigger_shape(pull, deadzone, curve, &points)),
                )
            })
            .collect::<Vec<_>>();
        let rt_points = (0..=64)
            .map(|index| {
                let pull = index as f32 / 64.0;
                to_pos(
                    pull,
                    boost_cap.powf(trigger_shape(pull, deadzone, curve, &points)),
                )
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
            let max_deadzone = points
                .first()
                .map(|point| (point.pull - 0.10).max(0.0))
                .unwrap_or(0.9)
                .min(0.9);
            let value = ((position.x - plot.min.x) / plot.width()).clamp(0.0, max_deadzone);
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

        let point_error = self.trigger_points_error();
        let border_color = if point_error.is_some() { ERROR } else { BORDER };
        painter.rect_stroke(
            plot,
            5.0,
            Stroke::new(1.0_f32, border_color),
            StrokeKind::Inside,
        );
        let plot_response = ui.interact(
            plot,
            ui.id().with((accel_path, "plot-points")),
            Sense::click(),
        );
        if plot_response.double_clicked()
            && points.len() < 5
            && let Some(position) = plot_response.interact_pointer_pos()
        {
            let pull = ((position.x - plot.min.x) / plot.width()).clamp(deadzone + 0.10, 0.90);
            if points.iter().all(|point| (point.pull - pull).abs() >= 0.10) {
                let output = trigger_shape(pull, deadzone, curve, &points);
                let mut next = points.clone();
                next.push(CurvePoint { pull, output });
                next.sort_by(|a, b| a.pull.total_cmp(&b.pull));
                self.selected_point = next
                    .iter()
                    .position(|point| (point.pull - pull).abs() < 1e-6);
                self.set_trigger_points(&next);
            }
        }

        let mut next_points = points.clone();
        let mut remove_point = None;
        let mut select = None;
        let mut move_point = false;
        for (index, point) in points.iter().enumerate() {
            let point_pos = to_pos(point.pull, point.output);
            let response = ui.interact(
                Rect::from_center_size(point_pos, Vec2::splat(15.0)),
                ui.id().with((accel_path, "point", index)),
                Sense::click_and_drag(),
            );
            let selected = self.selected_point == Some(index);
            painter.circle_filled(
                point_pos,
                if selected { 5.5 } else { 4.0 },
                if selected { Color32::WHITE } else { ACCENT },
            );
            painter.circle_stroke(
                point_pos,
                if selected { 7.5 } else { 6.0 },
                Stroke::new(1.0_f32, ACCENT),
            );
            painter.text(
                Pos2::new(point_pos.x + 8.0, point_pos.y - 4.0),
                Align2::LEFT_BOTTOM,
                format!("P{}", index + 1),
                FontId::monospace(8.0),
                TEXT_DIM,
            );
            if response.clicked() {
                select = Some(index);
            }
            if response.secondary_clicked() {
                remove_point = Some(index);
            }
            if response.dragged()
                && let Some(position) = response.interact_pointer_pos()
            {
                let snap = if ui.input(|input| input.modifiers.alt) {
                    0.001
                } else {
                    0.01
                };
                let mut pull =
                    ((position.x - plot.min.x) / plot.width()).clamp(deadzone + 0.001, 0.999);
                let mut output = ((plot.max.y - position.y) / plot.height()).clamp(0.0, 1.0);
                pull = (pull / snap).round() * snap;
                output = (output / snap).round() * snap;
                let previous_pull = index
                    .checked_sub(1)
                    .map(|previous| points[previous].pull)
                    .unwrap_or(deadzone);
                let next_pull = points.get(index + 1).map(|next| next.pull).unwrap_or(1.0);
                let previous_output = index
                    .checked_sub(1)
                    .map(|previous| points[previous].output)
                    .unwrap_or(0.0);
                let next_output = points.get(index + 1).map(|next| next.output).unwrap_or(1.0);
                if next_pull - previous_pull >= 0.20 {
                    next_points[index].pull = pull.clamp(previous_pull + 0.10, next_pull - 0.10);
                }
                next_points[index].output = output.clamp(previous_output, next_output);
                move_point = true;
            }
            if response.hovered() {
                response.on_hover_text(format!(
                    "P{}: pull {:.0}% → shape {:.0}% (right-click to delete)",
                    index + 1,
                    point.pull * 100.0,
                    point.output * 100.0
                ));
            }
        }
        if let Some(index) = select {
            self.selected_point = Some(index);
        }
        if let Some(index) = remove_point {
            next_points.remove(index);
            self.set_trigger_points(&next_points);
            self.selected_point = None;
        } else if move_point {
            self.set_trigger_points(&next_points);
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
                RichText::new("Simulated pull · offline")
                    .size(10.0)
                    .color(TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Reset").clicked() {
                    self.sim_lt = 0.0;
                    self.sim_rt = 0.0;
                    self.sim_last_left = true;
                    self.sim_first_left = true;
                }
            });
        });
        ui.add_space(4.0);
        let before_lt = self.sim_lt;
        let before_rt = self.sim_rt;
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
        if (self.sim_lt - before_lt).abs() > 1e-6 {
            self.sim_last_left = true;
        }
        if (self.sim_rt - before_rt).abs() > 1e-6 {
            self.sim_last_left = false;
        }
        let before_lt_active = before_lt > deadzone;
        let before_rt_active = before_rt > deadzone;
        let lt_active = self.sim_lt > deadzone;
        let rt_active = self.sim_rt > deadzone;
        if !before_lt_active && lt_active && !before_rt_active {
            self.sim_first_left = true;
        }
        if !before_rt_active && rt_active && !before_lt_active {
            self.sim_first_left = false;
        }
        if lt_active && !rt_active {
            self.sim_first_left = true;
        } else if !lt_active && rt_active {
            self.sim_first_left = false;
        } else if !lt_active && !rt_active {
            self.sim_first_left = true;
        }
        let socd = get_path(&self.model, "gamepad.navi.pointer.speed_socd")
            .and_then(Value::as_str)
            .unwrap_or("multiply");
        let pick = |prefer_left: bool| match (lt_active, rt_active) {
            (true, true) => Some(prefer_left),
            (true, false) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        };
        let side_inputs = |left: Option<bool>| match left {
            Some(true) => (self.sim_lt, 0.0),
            Some(false) => (0.0, self.sim_rt),
            None => (0.0, 0.0),
        };
        let (effective_lt, effective_rt) = match socd {
            "last" => side_inputs(pick(self.sim_last_left)),
            "first" => side_inputs(pick(self.sim_first_left)),
            "cancel" if lt_active && rt_active => (0.0, 0.0),
            "prio_decel" if lt_active => (self.sim_lt, 0.0),
            "prio_accel" if rt_active => (0.0, self.sim_rt),
            _ => (self.sim_lt, self.sim_rt),
        };
        let points = self.trigger_points();
        let lt_shape = trigger_shape(effective_lt, deadzone, curve, &points);
        let rt_shape = trigger_shape(effective_rt, deadzone, curve, &points);
        let pointer_multiplier = if pointer_enabled {
            trigger_multiplier(
                pointer_floor,
                pointer_cap,
                effective_lt,
                effective_rt,
                deadzone,
                curve,
                &points,
            )
        } else {
            1.0
        };
        let scroll_multiplier = if scroll_enabled {
            trigger_multiplier(
                scroll_floor,
                scroll_cap,
                effective_lt,
                effective_rt,
                deadzone,
                curve,
                &points,
            )
        } else {
            1.0
        };
        ui.label(
            RichText::new(format!(
                "Target speed vs baseline (no smoothing · speed {socd})"
            ))
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
    }

    fn accel_fields(&mut self, ui: &mut Ui) {
        let pointer = "gamepad.navi.pointer";
        if ui.available_width() >= 660.0 {
            ui.columns(3, |cols| {
                mini_heading(&mut cols[0], "Trigger");
                self.trigger_deadzone_field(&mut cols[0]);
                self.f32_field(
                    &mut cols[0],
                    &format!("{pointer}.trigger_curve"),
                    "Shape curve",
                    0.05,
                    5.0,
                    0.05,
                );
                self.combo_field(
                    &mut cols[0],
                    &format!("{pointer}.speed_socd"),
                    "Speed SOCD",
                    &[
                        "last",
                        "first",
                        "cancel",
                        "multiply",
                        "prio_decel",
                        "prio_accel",
                    ],
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
            self.trigger_deadzone_field(ui);
            self.f32_field(
                ui,
                &format!("{pointer}.trigger_curve"),
                "Shape curve",
                0.05,
                5.0,
                0.05,
            );
            self.combo_field(
                ui,
                &format!("{pointer}.speed_socd"),
                "Speed SOCD",
                &[
                    "last",
                    "first",
                    "cancel",
                    "multiply",
                    "prio_decel",
                    "prio_accel",
                ],
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

    fn behavior_fields(&mut self, ui: &mut Ui) {
        if ui.available_width() >= 900.0 {
            ui.columns(3, |cols| {
                self.behavior_input(&mut cols[0]);
                self.behavior_close(&mut cols[1]);
                self.behavior_drop(&mut cols[2]);
            });
        } else {
            self.behavior_input(ui);
            ui.add_space(8.0);
            self.behavior_close(ui);
            ui.add_space(8.0);
            self.behavior_drop(ui);
        }
    }

    fn behavior_input(&mut self, ui: &mut Ui) {
        mini_heading(ui, "Input");
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
    }

    fn behavior_close(&mut self, ui: &mut Ui) {
        mini_heading(ui, "Close when");
        self.bool_field(ui, "behavior.close_on_outside_click", "Outside click");
        self.bool_field(ui, "behavior.replay_outside_click", "Replay outside");
        self.bool_field(ui, "behavior.close_on_window_click", "Window click");
        self.bool_field(ui, "behavior.close_on_workspace_click", "Workspace click");
        self.bool_field(ui, "behavior.close_on_drop", "Drop");
        self.bool_field(
            ui,
            "behavior.close_on_external_focus_change",
            "External focus",
        );
        self.bool_field(ui, "behavior.close_on_escape", "Escape");
    }

    fn behavior_drop(&mut self, ui: &mut Ui) {
        mini_heading(ui, "Drop behavior");
        self.bool_field(ui, "behavior.empty_drop_floats", "Empty drop floats");
        self.bool_field(ui, "behavior.floating_drop_tiles", "Floating drop tiles");
        self.bool_field(ui, "behavior.focus_after_drop", "Focus after drop");
        self.bool_field(
            ui,
            "behavior.snap_cursor_after_drop",
            "Snap cursor after drop",
        );
    }

    fn popup_section(&mut self, ui: &mut Ui) {
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
        section_card(ui, "Behavior", |ui| self.behavior_fields(ui));
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

    fn appearance_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        if ui.available_width() >= 1000.0 {
            ui.columns(2, |cols| {
                self.style_fields(&mut cols[0]);
                self.bars_fields(&mut cols[1]);
            });
        } else {
            self.style_fields(ui);
            self.bars_fields(ui);
        }
    }

    fn style_fields(&mut self, ui: &mut Ui) {
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

    fn bars_fields(&mut self, ui: &mut Ui) {
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
            if tt(ui.button("Add bar"), "ui.action.add_bar").clicked()
                && let Some(bars) = self.model["bars"].as_array_mut()
            {
                bars.push(default_bar());
            }
        });
    }

    fn text_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
            ui.checkbox(&mut value, "").changed()
        });
        if changed {
            set_json_path(&mut self.model, path, Value::Bool(value));
        }
    }

    fn f32_field(&mut self, ui: &mut Ui, path: &str, label: &str, min: f64, max: f64, speed: f64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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

    /// Multi-select for button-code lists such as the open combo, the mode
    /// toggle, and layer hold buttons.
    fn button_list_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let values = get_path(&self.model, path)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut removed = None;
        let mut added = None;
        field_row_path(ui, Some(path), label, |ui| {
            for (index, value) in values.iter().enumerate() {
                let mut text = RichText::new(format!("{value}  ×")).monospace().size(10.5);
                if !BUTTONS.iter().any(|(name, _)| *name == value.as_str()) {
                    text = text.color(WARN);
                }
                if ui.small_button(text).on_hover_text("Remove").clicked() {
                    removed = Some(index);
                }
            }
            added = button_menu(ui, "+ Add", &values);
            removed.is_some() || added.is_some()
        });
        if let Some(index) = removed {
            if let Some(items) = get_path_mut(&mut self.model, path).and_then(Value::as_array_mut)
                && index < items.len()
            {
                items.remove(index);
            }
        } else if let Some(button) = added
            && let Some(items) = get_path_mut(&mut self.model, path).and_then(Value::as_array_mut)
        {
            items.push(json!(button));
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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
        let changed = field_row_path(ui, Some(path), label, |ui| {
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

    /// XMB and Navi lists, with the row for the selected control highlighted.
    /// Returns that row's rect so the card can link it to the controller.
    fn merged_bindings(&mut self, ui: &mut Ui) -> Option<Rect> {
        let target = self.selected_code();
        let mut rect = None;
        match self.context.as_str() {
            "xmb" => {
                rect = self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons", target);
                ui.add_space(12.0);
                self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base", None);
            }
            "base" => {
                self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons", None);
                ui.add_space(12.0);
                rect = self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base", target);
            }
            _ => {
                self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons", None);
                ui.add_space(12.0);
                self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base", None);
            }
        }
        rect
    }

    fn bindings_ui(
        &mut self,
        ui: &mut Ui,
        path: &str,
        actions: &[&str],
        title: &str,
        highlight: Option<&str>,
    ) -> Option<Rect> {
        let entries = get_path(&self.model, path)
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(code, action)| (code.clone(), action.as_str().unwrap_or("").to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        ui.horizontal(|ui| {
            let title_response = ui.add(
                egui::Label::new(RichText::new(title).size(11.5).strong()).sense(Sense::hover()),
            );
            if let Some(hover) = i18n::hover_for(None, title) {
                title_response.on_hover_text(hover);
            }
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
        let mut target = None;
        if entries.is_empty() {
            ui.label(RichText::new("No bindings").size(11.0).color(TEXT_MUTED));
        } else if ui.available_width() >= 640.0 && entries.len() > 1 {
            let left = entries.len().div_ceil(2);
            ui.columns(2, |cols| {
                for (index, (code, action)) in entries.iter().enumerate() {
                    let column = if index < left { 0 } else { 1 };
                    if let Some(rect) =
                        self.binding_row(&mut cols[column], path, actions, code, action, highlight)
                    {
                        target = Some(rect);
                    }
                }
            });
        } else {
            for (code, action) in &entries {
                if let Some(rect) = self.binding_row(ui, path, actions, code, action, highlight) {
                    target = Some(rect);
                }
            }
        }
        let used = entries
            .iter()
            .map(|(code, _)| code.clone())
            .collect::<Vec<_>>();
        if let Some(code) = button_menu(ui, "Add binding", &used) {
            set_map_value(&mut self.model, path, code, actions[0]);
        }
        target
    }

    fn binding_row(
        &mut self,
        ui: &mut Ui,
        path: &str,
        actions: &[&str],
        code: &str,
        action: &str,
        highlight: Option<&str>,
    ) -> Option<Rect> {
        let mut next_code = code.to_string();
        let mut next_action = action.to_string();
        let mut remove = false;
        let selected = highlight == Some(code);
        // Every other row's code, so the dropdown only offers free buttons.
        let used = get_path(&self.model, path)
            .and_then(Value::as_object)
            .map(|map| {
                map.keys()
                    .filter(|key| key.as_str() != code)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let frame = egui::Frame::new()
            .fill(if selected {
                Color32::from_rgba_unmultiplied(116, 162, 255, 30)
            } else {
                Color32::TRANSPARENT
            })
            .stroke(if selected {
                Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(116, 162, 255, 110))
            } else {
                Stroke::new(1.0_f32, Color32::TRANSPARENT)
            })
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(4, 1))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut code_changed = false;
                    ComboBox::from_id_salt(("binding-code", path, code))
                        .width(120.0)
                        .selected_text(RichText::new(&next_code).monospace())
                        .show_ui(ui, |ui| {
                            if !BUTTONS.iter().any(|(name, _)| *name == next_code) {
                                let current = next_code.clone();
                                ui.selectable_value(
                                    &mut next_code,
                                    current.clone(),
                                    RichText::new(current).monospace(),
                                )
                                .on_hover_text("Custom button code from the config");
                            }
                            for (option, label) in button_options(&used) {
                                code_changed |= ui
                                    .selectable_value(
                                        &mut next_code,
                                        option.to_string(),
                                        RichText::new(format!("{option}   {label}"))
                                            .monospace()
                                            .size(11.0),
                                    )
                                    .changed();
                            }
                        });
                    let mut action_changed = false;
                    ComboBox::from_id_salt(("binding", path, code))
                        .width((ui.available_width() - 34.0).clamp(100.0, 300.0))
                        .selected_text(&next_action)
                        .show_ui(ui, |ui| {
                            for option in actions {
                                action_changed |= ui
                                    .selectable_value(
                                        &mut next_action,
                                        (*option).to_string(),
                                        *option,
                                    )
                                    .changed();
                            }
                        });
                    remove = ui.small_button("x").clicked();
                    code_changed || action_changed
                })
                .inner
            });
        if remove {
            remove_map_key(&mut self.model, path, code);
        } else if next_code != code {
            remove_map_key(&mut self.model, path, code);
            set_map_value(&mut self.model, path, &next_code, &next_action);
        } else if frame.inner || next_action != action {
            set_map_value(&mut self.model, path, code, &next_action);
        }
        selected.then_some(frame.response.rect)
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
            self.button_list_field(ui, &format!("{path}.hold"), "Hold buttons");
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
            if tt(ui.button("Add hold axis"), "ui.action.add_hold_axis").clicked()
                && let Some(axes) =
                    self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"].as_array_mut()
            {
                axes.push(json!({"axis": "ABS_Z", "threshold": 0.35}));
            }
            self.bindings_ui(ui, &format!("{path}.bindings"), ACTIONS, "Bindings", None);
        }
        if let Some(name) = remove_layer
            && let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object_mut()
        {
            layers.remove(&name);
        }
        if tt(ui.button("Add layer"), "ui.action.add_layer").clicked() {
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
        if tt(ui.button("Add placement"), "ui.action.add_placement").clicked()
            && let Some(items) = self.model["popup"]["placements"].as_array_mut()
        {
            items.push(json!({"output": "", "x": "center", "y": "center", "margin": 48.0}));
        }
    }

    fn live_xmb_preview(&mut self, ui: &mut Ui, max: Vec2) {
        let live = self
            .live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default();
        let Some(snapshot) = snapshot_from_json(&live.snapshot) else {
            ui.label(
                RichText::new("Waiting for niri-xmb live snapshot")
                    .size(10.0)
                    .color(TEXT_DIM),
            );
            return;
        };
        if snapshot.outputs.is_empty() {
            ui.label(
                RichText::new("No outputs in live snapshot")
                    .size(10.0)
                    .color(TEXT_DIM),
            );
            return;
        }
        let Ok(config) = serde_json::from_value::<Config>(self.model.clone()) else {
            ui.label(
                RichText::new("Preview cannot read the current config")
                    .size(10.0)
                    .color(ERROR),
            );
            return;
        };
        let live_selection = PreviewSelection::from_live(&live.status, &snapshot);
        let landed = self.preview_selection.as_ref().is_some_and(|pending| {
            live_selection.as_ref().is_some_and(|live| {
                live.output == pending.output
                    && live.workspace_id == pending.workspace_id
                    && pending
                        .window_id
                        .is_none_or(|window| live.window_id == Some(window))
            })
        });
        if landed || self.preview_selection_at.elapsed() > Duration::from_millis(1800) {
            self.preview_selection = None;
        }
        let selection = self.preview_selection.clone().or(live_selection);
        let mode = live.status["mode"].as_str().unwrap_or("hidden");
        let input_active = matches!(mode, "manual" | "peek_armed");
        let key = format!(
            "{}\n{}\n{:?}\n{}\n{input_active}",
            self.model, live.snapshot, selection, mode
        );
        if self.preview.is_none() {
            self.preview = Some(PopupPreview::new());
        }
        if let Some(preview) = self.preview.as_mut() {
            preview.set_input_active(input_active);
        }
        if key != self.preview_key || self.preview_texture.is_none() {
            self.preview_key = key;
            self.render_live_preview(ui.ctx(), &config, &snapshot, selection.as_ref());
        }
        let Some(frame) = self.preview_frame.as_ref() else {
            ui.label(
                RichText::new("Popup could not be rendered")
                    .size(10.0)
                    .color(TEXT_DIM),
            );
            return;
        };
        let image_size = Vec2::new(frame.width.max(1) as f32, frame.height.max(1) as f32);
        let scale = (max.x / image_size.x).min(max.y / image_size.y).min(1.0);
        if scale <= 0.0 {
            return;
        }
        let size = image_size * scale;
        let stage = Vec2::new(max.x.max(size.x), max.y.max(size.y + 8.0));
        let (stage_rect, response) = ui.allocate_exact_size(stage, Sense::click());
        let rect = Rect::from_center_size(stage_rect.center(), size);

        let pointer = response.hover_pos().and_then(|pos| {
            rect.contains(pos).then(|| {
                (
                    (pos.x - rect.min.x) / rect.width() * image_size.x,
                    (pos.y - rect.min.y) / rect.height() * image_size.y,
                )
            })
        });
        let target =
            pointer.and_then(|(x, y)| self.preview_frame.as_ref()?.target_at(x, y).cloned());
        if target != self.preview_hover {
            self.preview_hover = target.clone();
            let changed = self
                .preview
                .as_mut()
                .is_some_and(|preview| preview.set_pointer(pointer));
            if changed {
                self.render_live_preview(ui.ctx(), &config, &snapshot, selection.as_ref());
            }
        }
        if matches!(
            target,
            Some(PreviewTarget::Window { .. } | PreviewTarget::Workspace { .. })
        ) {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }

        if let Some(texture) = self.preview_texture.clone() {
            ui.painter().image(
                texture.id(),
                rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        if response.clicked()
            && let Some(pos) = response.interact_pointer_pos()
        {
            let click = (
                (pos.x - rect.min.x) / rect.width() * image_size.x,
                (pos.y - rect.min.y) / rect.height() * image_size.y,
            );
            let clicked = self
                .preview_frame
                .as_ref()
                .and_then(|frame| frame.target_at(click.0, click.1).cloned());
            if let Some(target) = clicked {
                let output = selection
                    .as_ref()
                    .map(|selection| selection.output.clone())
                    .or_else(|| snapshot.focused_output().map(str::to_string))
                    .or_else(|| snapshot.outputs.keys().next().cloned());
                self.preview_click(target, output);
            }
        }
    }

    fn render_live_preview(
        &mut self,
        ctx: &egui::Context,
        config: &Config,
        snapshot: &niri_xmb::model::Snapshot,
        selection: Option<&PreviewSelection>,
    ) {
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        let Some(frame) = preview.render(config, snapshot, selection) else {
            self.preview_texture = None;
            self.preview_frame = None;
            return;
        };
        let image = ColorImage::from_rgba_premultiplied(
            [frame.width as usize, frame.height as usize],
            &frame.rgba,
        );
        self.preview_texture =
            Some(ctx.load_texture("xmb-popup-preview", image, TextureOptions::LINEAR));
        self.preview_frame = Some(frame);
    }

    fn preview_click(&mut self, target: PreviewTarget, output: Option<String>) {
        match target {
            PreviewTarget::Window {
                workspace_id,
                window_id,
            } => {
                let _ = Command::new(&self.niri_xmb)
                    .args(["focus-window", "--window-id", &window_id.to_string()])
                    .spawn();
                if let Some(output) = output {
                    self.preview_selection = Some(PreviewSelection {
                        output,
                        workspace_id,
                        window_id: Some(window_id),
                    });
                    self.preview_selection_at = Instant::now();
                }
            }
            PreviewTarget::Workspace { workspace_id } => {
                let _ = Command::new(&self.niri_xmb)
                    .args([
                        "focus-workspace",
                        "--workspace-id",
                        &workspace_id.to_string(),
                    ])
                    .spawn();
                if let Some(output) = output {
                    self.preview_selection = Some(PreviewSelection {
                        output,
                        workspace_id,
                        window_id: None,
                    });
                    self.preview_selection_at = Instant::now();
                }
            }
            PreviewTarget::Panel | PreviewTarget::Canvas { .. } => {}
        }
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

// ---------------------------------------------------------------------------
// Persistent live dock
// ---------------------------------------------------------------------------

/// Controller button and axis state aggregated from `niri-xmb status`.
#[derive(Default, Clone)]
struct GamepadLiveView {
    buttons: BTreeSet<String>,
    axes: BTreeMap<String, f32>,
}

impl GamepadLiveView {
    fn from_status(status: &Value) -> Self {
        let mut view = Self::default();
        for device in status["gamepad"]["live"].as_array().into_iter().flatten() {
            for button in device["buttons"].as_array().into_iter().flatten() {
                if let Some(button) = button.as_str() {
                    view.buttons.insert(button.to_string());
                }
            }
            for (axis, value) in device["axes"].as_object().into_iter().flatten() {
                let value = value.as_f64().unwrap_or(0.0) as f32;
                let entry = view.axes.entry(axis.clone()).or_insert(0.0);
                if value.abs() > entry.abs() {
                    *entry = value;
                }
            }
        }
        view
    }

    fn button(&self, code: &str) -> bool {
        self.buttons.contains(code)
    }

    fn dn_axis(&self, axis: &str) -> f32 {
        self.axes.get(axis).copied().unwrap_or(0.0)
    }

    fn hat(&self, axis: &str, positive: bool) -> bool {
        let value = self.dn_axis(axis);
        if positive { value > 0.5 } else { value < -0.5 }
    }

    fn trigger(&self, axis: &str, button: &str) -> f32 {
        self.dn_axis(axis)
            .max(if self.button(button) { 1.0 } else { 0.0 })
    }

    fn dpad(&self, direction: &str) -> bool {
        let code = match direction {
            "up" => "BTN_DPAD_UP",
            "down" => "BTN_DPAD_DOWN",
            "left" => "BTN_DPAD_LEFT",
            "right" => "BTN_DPAD_RIGHT",
            _ => return false,
        };
        self.button(code)
            || match direction {
                "up" => self.hat("ABS_HAT0Y", false),
                "down" => self.hat("ABS_HAT0Y", true),
                "left" => self.hat("ABS_HAT0X", false),
                "right" => self.hat("ABS_HAT0X", true),
                _ => false,
            }
    }
}

struct ControllerRow {
    label: String,
    code: String,
    pressed: bool,
    trigger: Option<f32>,
    hold_axis: Option<&'static str>,
}

impl StudioApp {
    fn live_dock(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::new()
            .fill(PANEL)
            .inner_margin(egui::Margin::symmetric(12, 4))
            .stroke(Stroke::new(1.0_f32, BORDER));
        let max_height = (ctx.screen_rect().height() * 0.62).max(180.0);
        self.dock_height = self.dock_height.clamp(170.0, max_height);
        if self.dock_collapsed {
            egui::TopBottomPanel::bottom("live-dock")
                .frame(frame)
                .exact_height(38.0)
                .show(ctx, |ui| self.dock_header(ui));
            return;
        }
        egui::TopBottomPanel::bottom("live-dock")
            .frame(frame)
            .exact_height(self.dock_height)
            .show(ctx, |ui| {
                self.dock_grip(ui, max_height);
                self.dock_header(ui);
                ui.add_space(4.0);
                let body = Vec2::new(ui.available_width(), ui.available_height().max(80.0));
                if self.dock_tab == DockTab::Keyboard
                    && self.keymap_at.elapsed() > Duration::from_secs(5)
                {
                    self.reload_keymap();
                }
                match self.dock_tab {
                    DockTab::Popup => self.dock_popup(ui, body),
                    DockTab::Controller => self.dock_controller(ui, body),
                    DockTab::Keyboard => self.dock_keyboard(ui, body),
                }
            });
    }

    /// Visible drag handle on the dock's top edge.
    ///
    /// egui's built-in panel border is only a few pixels wide and invisible, so
    /// the owner could not find the resize affordance. This strip owns the
    /// height directly and paints a grip for it.
    fn dock_grip(&mut self, ui: &mut Ui, max_height: f32) {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::drag());
        let active = response.hovered() || response.dragged();
        ui.painter().rect_filled(
            Rect::from_center_size(rect.center(), Vec2::new(56.0, 5.0)),
            2.5,
            if active {
                ACCENT
            } else {
                Color32::from_rgb(92, 102, 118)
            },
        );
        if active {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
        }
        if response.dragged() {
            let delta = ui.ctx().input(|input| input.pointer.delta().y);
            self.dock_height = (self.dock_height - delta).clamp(170.0, max_height);
        }
        if response.double_clicked() {
            self.dock_collapsed = true;
        }
        response.on_hover_text("Drag to resize · double-click to hide");
    }

    fn dock_header(&mut self, ui: &mut Ui) {
        if self.dock_section != self.section {
            self.dock_section = self.section;
            if self.dock_follow {
                self.dock_tab = DockTab::for_section(self.section);
            }
        }
        ui.horizontal(|ui| {
            let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
            let live = self
                .live
                .lock()
                .map(|live| live.clone())
                .unwrap_or_default();
            let color = if live.status.is_null() { WARN } else { OK };
            ui.painter().circle_filled(dot.center(), 3.5, color);
            ui.label(RichText::new("Live").size(12.0).strong());
            ui.add_space(6.0);
            for tab in DockTab::ALL {
                if dock_tab_button(ui, self.dock_tab == tab, tab.label()).clicked() {
                    self.dock_tab = tab;
                    if tab == DockTab::Keyboard {
                        // The key map needs vertical room to stay square.
                        self.dock_height = self.dock_height.max(380.0);
                    }
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let label = if self.dock_collapsed { "Show" } else { "Hide" };
                if ui
                    .add(egui::Button::new(label).small())
                    .on_hover_text(if self.dock_collapsed {
                        "Expand live dock"
                    } else {
                        "Collapse live dock"
                    })
                    .clicked()
                {
                    self.dock_collapsed = !self.dock_collapsed;
                }
                if ui
                    .checkbox(&mut self.dock_follow, RichText::new("Follow").size(10.5))
                    .on_hover_text("Switch the dock tab with the editor section")
                    .clicked()
                    && self.dock_follow
                {
                    self.dock_tab = DockTab::for_section(self.section);
                }
                let status = self.dock_status_text(&live);
                ui.label(RichText::new(status).monospace().size(9.5).color(TEXT_DIM));
            });
        });
    }

    fn dock_status_text(&self, live: &LiveState) -> String {
        if live.status.is_null() {
            return if live.error.is_empty() {
                "waiting for daemon".into()
            } else {
                live.error.clone()
            };
        }
        let mut text = format!(
            "daemon {}",
            live.status["mode"].as_str().unwrap_or("hidden")
        );
        if let Some(selection) = live.status["selection"].as_object() {
            if let Some(output) = selection["output"].as_str() {
                text.push_str(&format!(" · {output}"));
            }
            if let Some(workspace) = selection["workspace_id"].as_u64() {
                text.push_str(&format!(" · ws {workspace}"));
            }
        }
        if let Some(generation) = live.status["generation"].as_u64() {
            text.push_str(&format!(" · gen {generation}"));
        }
        text
    }

    fn dock_popup(&mut self, ui: &mut Ui, body: Vec2) {
        let preview_w = (body.x * 0.58).clamp(240.0, body.x);
        let preview_h = (body.y - 4.0).max(70.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                Vec2::new(preview_w, preview_h),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.set_min_height(preview_h);
                    self.live_xmb_preview(ui, Vec2::new(preview_w, preview_h));
                },
            );
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("Panel").size(11.5).strong());
                ui.add_space(2.0);
                ui.horizontal_wrapped(|ui| {
                    metric_chip(
                        ui,
                        "outputs",
                        self.model["popup"]["outputs"].as_str().unwrap_or("focused"),
                    );
                    metric_chip(
                        ui,
                        "position",
                        self.model["popup"]["position"].as_str().unwrap_or("cursor"),
                    );
                    metric_chip(
                        ui,
                        "surface",
                        self.model["popup"]["surface_mode"]
                            .as_str()
                            .unwrap_or("fixed"),
                    );
                    metric_chip(
                        ui,
                        "labels",
                        on_off(self.model["popup"]["show_labels"].as_bool()),
                    );
                    metric_chip(
                        ui,
                        "titles",
                        on_off(self.model["popup"]["show_titles"].as_bool()),
                    );
                    metric_chip(
                        ui,
                        "anim",
                        format!(
                            "{} ms",
                            self.model["animation"]["panel_ms"].as_f64().unwrap_or(0.0)
                        ),
                    );
                });
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    metric_chip(ui, "bars", array_len(&self.model["bars"]));
                    metric_chip(
                        ui,
                        "placements",
                        array_len(&self.model["popup"]["placements"]),
                    );
                    metric_chip(
                        ui,
                        "navi binds",
                        map_len(&self.model["gamepad"]["navi"]["bindings"]),
                    );
                    metric_chip(ui, "xmb binds", map_len(&self.model["gamepad"]["buttons"]));
                });
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Same renderer as the daemon · hover to preview · click an icon to focus",
                    )
                    .size(9.5)
                    .color(TEXT_DIM),
                );
            });
        });
    }

    fn dock_controller(&mut self, ui: &mut Ui, body: Vec2) {
        let live = self
            .live
            .lock()
            .map(|live| GamepadLiveView::from_status(&live.status))
            .unwrap_or_default();
        let rows = self.controller_rows(&live);
        let map_h = (body.y - 4.0).clamp(100.0, 300.0);
        let map_w = (map_h / self.template.aspect()).min((body.x * 0.55).max(220.0));
        ui.horizontal_top(|ui| {
            self.controller_live_map(ui, Vec2::new(map_w, map_h), &live);
            ui.add_space(10.0);
            ui.vertical(|ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Controls").size(11.5).strong());
                    ui.add_space(4.0);
                    for template in [Template::Xbox, Template::Ds4] {
                        if ui
                            .selectable_label(self.template == template, template.label())
                            .clicked()
                        {
                            self.switch_template(template);
                        }
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} connected · {}",
                                self.connected_count(),
                                self.gamepad_mode_name()
                            ))
                            .monospace()
                            .size(9.0)
                            .color(TEXT_DIM),
                        );
                    });
                });
                if self.connected_count() > 0 && !self.gamepad_live_available() {
                    ui.label(
                        RichText::new(
                            "Live presses unavailable: the running daemon predates gamepad.live",
                        )
                        .size(9.0)
                        .color(WARN),
                    );
                }
                ui.add_space(2.0);
                for (label, axis, button) in
                    [("LT", "ABS_Z", "BTN_TL2"), ("RT", "ABS_RZ", "BTN_TR2")]
                {
                    let value = live.trigger(axis, button);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(label).monospace().size(9.0).color(TEXT_DIM));
                        let width = (ui.available_width() - 44.0).max(60.0);
                        let (bar, _) =
                            ui.allocate_exact_size(Vec2::new(width, 5.0), Sense::hover());
                        ui.painter().rect_filled(bar, 2.5, FIELD_BG);
                        let fill = Rect::from_min_size(
                            bar.min,
                            Vec2::new(bar.width() * value.clamp(0.0, 1.0), bar.height()),
                        );
                        ui.painter().rect_filled(
                            fill,
                            2.5,
                            if value > 0.05 { ACCENT } else { BORDER },
                        );
                        ui.label(
                            RichText::new(format!("{:>3.0}%", value * 100.0))
                                .monospace()
                                .size(9.0)
                                .color(TEXT_DIM),
                        );
                    });
                }
                ui.add_space(2.0);
                let list_w = ui.available_width();
                let columns = controller_columns(list_w);
                let label_w = columns.label;
                let binding_w = columns.binding;
                let layer_w = columns.layers;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.add_space(17.0);
                    column_header(ui, "Control", label_w);
                    column_header(ui, "Binding", binding_w);
                    column_header(ui, "Layers", layer_w);
                });
                ui.separator();
                ScrollArea::vertical()
                    .id_salt("dock-controller-list")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        for row in &rows {
                            let binding = self.controller_binding(&row.code);
                            let layers = self.controller_layers(&row.code, row.hold_axis);
                            controller_row(
                                ui,
                                row,
                                binding.as_deref(),
                                &layers,
                                label_w,
                                binding_w,
                                layer_w,
                            );
                        }
                    });
            });
        });
    }

    fn connected_count(&self) -> usize {
        self.live
            .lock()
            .map(|live| {
                live.status["gamepad"]["connected"]
                    .as_array()
                    .map_or(0, Vec::len)
            })
            .unwrap_or(0)
    }

    fn gamepad_live_available(&self) -> bool {
        self.live
            .lock()
            .map(|live| live.status["gamepad"]["live"].is_array())
            .unwrap_or(false)
    }

    fn gamepad_mode_name(&self) -> &'static str {
        let mode = self
            .live
            .lock()
            .ok()
            .and_then(|live| live.status["gamepad"]["mode"].as_str().map(str::to_string));
        match mode.as_deref() {
            Some("navi") => "navi",
            Some("game") => "game",
            _ => "off",
        }
    }

    fn controller_rows(&self, live: &GamepadLiveView) -> Vec<ControllerRow> {
        let mut rows = Vec::new();
        for hotspot in self.template.hotspots() {
            match hotspot.control {
                "D-PAD" => {
                    for (label, code, direction) in [
                        ("D-Pad Up", "BTN_DPAD_UP", "up"),
                        ("D-Pad Down", "BTN_DPAD_DOWN", "down"),
                        ("D-Pad Left", "BTN_DPAD_LEFT", "left"),
                        ("D-Pad Right", "BTN_DPAD_RIGHT", "right"),
                    ] {
                        rows.push(ControllerRow {
                            label: label.into(),
                            code: code.into(),
                            pressed: live.dpad(direction),
                            trigger: None,
                            hold_axis: None,
                        });
                    }
                }
                "LT" | "L2" => rows.push(ControllerRow {
                    label: if hotspot.control == "LT" { "LT" } else { "L2" }.into(),
                    code: "BTN_TL2".into(),
                    pressed: live.trigger("ABS_Z", "BTN_TL2") > 0.05,
                    trigger: Some(live.trigger("ABS_Z", "BTN_TL2")),
                    hold_axis: Some("ABS_Z"),
                }),
                "RT" | "R2" => rows.push(ControllerRow {
                    label: if hotspot.control == "RT" { "RT" } else { "R2" }.into(),
                    code: "BTN_TR2".into(),
                    pressed: live.trigger("ABS_RZ", "BTN_TR2") > 0.05,
                    trigger: Some(live.trigger("ABS_RZ", "BTN_TR2")),
                    hold_axis: Some("ABS_RZ"),
                }),
                "L STICK" => rows.push(ControllerRow {
                    label: "L3".into(),
                    code: "BTN_THUMBL".into(),
                    pressed: live.button("BTN_THUMBL"),
                    trigger: None,
                    hold_axis: None,
                }),
                "R STICK" => rows.push(ControllerRow {
                    label: "R3".into(),
                    code: "BTN_THUMBR".into(),
                    pressed: live.button("BTN_THUMBR"),
                    trigger: None,
                    hold_axis: None,
                }),
                control => {
                    let Some(code) = hotspot.code else {
                        continue;
                    };
                    rows.push(ControllerRow {
                        label: control.into(),
                        code: code.into(),
                        pressed: live.button(code),
                        trigger: None,
                        hold_axis: None,
                    });
                }
            }
        }
        rows
    }

    fn controller_binding(&self, code: &str) -> Option<String> {
        self.model["gamepad"]["navi"]["bindings"][code]
            .as_str()
            .or_else(|| self.model["gamepad"]["buttons"][code].as_str())
            .map(str::to_string)
    }

    fn controller_layers(&self, code: &str, axis: Option<&str>) -> Vec<String> {
        let mut names = Vec::new();
        let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object() else {
            return names;
        };
        for (name, layer) in layers {
            let holds = layer["hold"]
                .as_array()
                .is_some_and(|hold| hold.iter().any(|value| value.as_str() == Some(code)));
            let holds_axis = axis.is_some_and(|axis| {
                layer["hold_axes"].as_array().is_some_and(|holds| {
                    holds.iter().any(|hold| hold["axis"].as_str() == Some(axis))
                })
            });
            let binds = layer["bindings"][code].is_string();
            if holds || holds_axis || binds {
                names.push(name.clone());
            }
        }
        names
    }

    fn controller_live_map(&self, ui: &mut Ui, size: Vec2, live: &GamepadLiveView) {
        let template = self.template;
        let (box_rect, _) = ui.allocate_exact_size(size, Sense::hover());
        if let Some(backdrop) = template.backdrop() {
            ui.painter().rect_filled(box_rect, 10.0, backdrop);
            ui.painter().rect_stroke(
                box_rect,
                10.0,
                Stroke::new(1.0_f32, BORDER),
                StrokeKind::Inside,
            );
        }
        let image_size = if size.x * template.aspect() <= size.y {
            Vec2::new(size.x, size.x * template.aspect())
        } else {
            Vec2::new(size.y / template.aspect(), size.y)
        };
        let rect = Rect::from_center_size(box_rect.center(), image_size);
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
        for hotspot in template.hotspots() {
            let center = Pos2::new(
                rect.min.x + hotspot.x * rect.width(),
                rect.min.y + hotspot.y * rect.height(),
            );
            let hit = Rect::from_center_size(
                center,
                Vec2::new(hotspot.w * rect.width(), hotspot.h * rect.width()),
            );
            let pressed = match hotspot.control {
                "LT" | "L2" => live.trigger("ABS_Z", "BTN_TL2") > 0.05,
                "RT" | "R2" => live.trigger("ABS_RZ", "BTN_TR2") > 0.05,
                "L STICK" => live.button("BTN_THUMBL"),
                "R STICK" => live.button("BTN_THUMBR"),
                "D-PAD" => ["up", "down", "left", "right"]
                    .iter()
                    .any(|direction| live.dpad(direction)),
                _ => hotspot.code.is_some_and(|code| live.button(code)),
            };
            let response = ui.interact(
                hit,
                ui.id()
                    .with(("dock-hotspot", template.label(), hotspot.control)),
                Sense::hover(),
            );
            if pressed {
                let color = Color32::from_rgba_unmultiplied(116, 162, 255, 70);
                if hotspot.circle {
                    ui.painter()
                        .circle_filled(center, hit.width().min(hit.height()) / 2.0, color);
                    ui.painter().circle_stroke(
                        center,
                        hit.width().min(hit.height()) / 2.0,
                        Stroke::new(1.8_f32, Color32::WHITE),
                    );
                } else {
                    ui.painter().rect_filled(hit, 7.0, color);
                    ui.painter().rect_stroke(
                        hit,
                        7.0,
                        Stroke::new(1.8_f32, Color32::WHITE),
                        StrokeKind::Inside,
                    );
                }
            }
            if response.hovered()
                && let Some(code) = hotspot.code
            {
                response
                    .clone()
                    .on_hover_text(match self.controller_binding(code) {
                        Some(binding) => format!("{} · {binding}", hotspot.control),
                        None => format!("{} · unbound", hotspot.control),
                    });
            }
        }
    }

    fn dock_keyboard(&mut self, ui: &mut Ui, body: Vec2) {
        let mapped_keys = self
            .keymap
            .effective()
            .filter_map(|bind| bind.key_id)
            .collect::<BTreeSet<_>>()
            .len();
        let extra_keys = self
            .keymap
            .effective()
            .filter(|bind| bind.key_id.is_none())
            .count();
        let duplicates = self
            .keymap
            .binds
            .iter()
            .filter(|bind| bind.duplicate)
            .count();
        self.keymap_toolbar(ui, mapped_keys, extra_keys, duplicates);
        ui.add_space(4.0);

        let pressed = pressed_key_ids(ui.ctx());
        let index = keymap_index(&self.keymap);
        let query = self.keymap_search.trim().to_ascii_lowercase();
        let selected = self.keymap_selected.clone();
        let mut filtered: Vec<&keymap::NiriBind> = self
            .keymap
            .binds
            .iter()
            .filter(|bind| selected.as_deref().is_none_or(|id| bind.key_id == Some(id)))
            .filter(|bind| query.is_empty() || keymap_matches(bind, &query))
            .collect();
        filtered.sort_by_key(|bind| bind.shadowed || bind.duplicate);

        let remaining = ui.available_size();
        let mut clicked = None;
        if body.x >= 880.0 {
            // Wide dock: board on the left, bind table on the right. The board
            // keeps its 15-key width and never stretches to fill the column.
            let cell_by_height = ((remaining.y - 24.0) / 6.0).clamp(12.0, 40.0);
            let map_w = (15.0 * cell_by_height + 18.0).clamp(320.0, 640.0);
            let list_w = (body.x - map_w - 14.0).max(260.0);
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(
                    Vec2::new(map_w, remaining.y),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_min_width(map_w);
                        clicked = self.keyboard_map(ui, &pressed, &index, &query);
                    },
                );
                ui.add_space(14.0);
                ui.allocate_ui_with_layout(
                    Vec2::new(list_w, remaining.y),
                    Layout::top_down(Align::Min),
                    |ui| {
                        ui.set_min_width(list_w);
                        keymap_list(ui, &filtered, remaining.y);
                    },
                );
            });
        } else {
            let map_h = (remaining.y * 0.55).max(150.0);
            ui.allocate_ui_with_layout(
                Vec2::new(remaining.x, map_h),
                Layout::top_down(Align::Min),
                |ui| {
                    clicked = self.keyboard_map(ui, &pressed, &index, &query);
                },
            );
            ui.add_space(6.0);
            let list_h = (remaining.y - map_h - 8.0).max(80.0);
            keymap_list(ui, &filtered, list_h);
        }
        if let Some(id) = clicked {
            self.keymap_selected = if self.keymap_selected.as_deref() == Some(id) {
                None
            } else {
                Some(id.to_string())
            };
        }
    }

    fn keymap_toolbar(
        &mut self,
        ui: &mut Ui,
        mapped_keys: usize,
        extra_keys: usize,
        duplicates: usize,
    ) {
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.keymap_search)
                    .hint_text("Filter action, hotkey, or title")
                    .desired_width(230.0),
            );
            if !self.keymap_search.is_empty() && ui.small_button("Clear").clicked() {
                self.keymap_search.clear();
            }
            if let Some(selected) = self.keymap_selected.clone()
                && ui
                    .button(format!("{}  ×", keymap::key_label(&selected)))
                    .on_hover_text("Clear the selected key")
                    .clicked()
            {
                self.keymap_selected = None;
            }
            if ui
                .button("Reload")
                .on_hover_text("Re-read the Niri config and its includes")
                .clicked()
            {
                self.reload_keymap();
            }
            let mut stats = format!(
                "{} binds · {mapped_keys} mapped keys · {extra_keys} extra keys · {} files",
                self.keymap.effective_count(),
                self.keymap.files.len()
            );
            if duplicates > 0 {
                stats.push_str(&format!(" · {duplicates} duplicate"));
            }
            if !self.keymap.errors.is_empty() {
                stats.push_str(&format!(" · {} errors", self.keymap.errors.len()));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(error) = self.keymap.errors.first() {
                    ui.label(RichText::new("config error").size(10.0).color(ERROR))
                        .on_hover_text(error);
                } else {
                    ui.label(
                        RichText::new(&stats)
                            .monospace()
                            .size(10.0)
                            .color(if duplicates > 0 { WARN } else { TEXT_DIM }),
                    );
                }
            });
        });
    }

    fn keyboard_map(
        &self,
        ui: &mut Ui,
        pressed: &BTreeSet<&'static str>,
        index: &HashMap<&'static str, Vec<&keymap::NiriBind>>,
        query: &str,
    ) -> Option<&'static str> {
        let avail = ui.available_size();
        let legend_h = keyboard_legend_height(avail);
        let cell = keyboard_cell(avail);
        let unit = cell / 4.0;
        let gap = (unit * 0.18).clamp(1.0, 2.8);
        let board_w = 15.0 * cell + 10.0;
        let board_h = 6.0 * cell + 10.0;
        let (outer, _) =
            ui.allocate_exact_size(Vec2::new(avail.x, board_h + legend_h + 6.0), Sense::hover());
        let board = Rect::from_min_size(
            Pos2::new(outer.center().x - board_w / 2.0, outer.min.y),
            Vec2::new(board_w, board_h),
        );
        let painter = ui.painter_at(outer);
        painter.rect_filled(board, 10.0, FIELD_BG);
        painter.rect_stroke(
            board,
            10.0,
            Stroke::new(1.0_f32, BORDER),
            StrokeKind::Inside,
        );
        let mut clicked = None;
        for (row_index, keys) in keymap::ANSI_ROWS.iter().enumerate() {
            let y = board.min.y + 5.0 + row_index as f32 * cell;
            let mut x = board.min.x + 5.0;
            for (id, units) in keys.iter() {
                let cap_width = *units as f32 * unit;
                if id.is_empty() {
                    x += cap_width;
                    continue;
                }
                let cap = Rect::from_min_size(
                    Pos2::new(x + gap * 0.5, y + gap * 0.5),
                    Vec2::new(cap_width - gap, cell - gap),
                );
                let binds = index.get(id).map(Vec::as_slice).unwrap_or_default();
                let bound = binds.len();
                let is_pressed = pressed.contains(id);
                let is_selected = self.keymap_selected.as_deref() == Some(*id);
                let search_hit =
                    !query.is_empty() && binds.iter().any(|bind| keymap_matches(bind, query));
                let (fill, stroke) = if is_pressed {
                    (
                        Color32::from_rgba_unmultiplied(116, 162, 255, 110),
                        Stroke::new(1.5_f32, Color32::WHITE),
                    )
                } else if is_selected {
                    (Color32::from_rgb(46, 63, 90), Stroke::new(1.4_f32, ACCENT))
                } else if search_hit {
                    (Color32::from_rgb(64, 53, 30), Stroke::new(1.2_f32, WARN))
                } else if bound > 0 {
                    (
                        Color32::from_rgb(38, 50, 70),
                        Stroke::new(1.0_f32, Color32::from_rgb(78, 112, 166)),
                    )
                } else {
                    (
                        Color32::from_rgb(24, 27, 33),
                        Stroke::new(1.0_f32, Color32::from_rgb(35, 40, 48)),
                    )
                };
                painter.rect_filled(cap, 4.0, fill);
                painter.rect_stroke(cap, 4.0, stroke, StrokeKind::Inside);
                if bound > 1 {
                    painter.circle_filled(Pos2::new(cap.max.x - 4.5, cap.max.y - 4.5), 2.0, ACCENT);
                }
                let label = keymap::key_label(id);
                if !label.is_empty() {
                    painter.text(
                        cap.center(),
                        Align2::CENTER_CENTER,
                        label,
                        FontId::monospace((cell * 0.31).clamp(7.0, 13.0)),
                        if is_pressed {
                            Color32::WHITE
                        } else {
                            TEXT_MUTED
                        },
                    );
                }
                let response = ui.interact(cap, ui.id().with(("dock-key", *id)), Sense::click());
                if response.clicked() {
                    clicked = Some(*id);
                }
                if response.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    response.on_hover_ui(|ui| {
                        ui.label(RichText::new(keymap::key_label(id)).strong().size(12.0));
                        if binds.is_empty() {
                            ui.label(RichText::new("Unbound").size(10.0).color(TEXT_DIM));
                        }
                        for bind in binds.iter().take(6) {
                            ui.label(
                                RichText::new(format!(
                                    "{}   {}",
                                    bind.hotkey_text(),
                                    bind.action_text()
                                ))
                                .monospace()
                                .size(10.0)
                                .color(if bind.shadowed {
                                    TEXT_DIM
                                } else {
                                    TEXT
                                }),
                            );
                        }
                    });
                }
                x += cap_width;
            }
        }
        // Legend keeps the colour language explicit instead of leaving the
        // owner to infer what the tinted caps mean.
        if legend_h == 0.0 {
            return clicked;
        }
        let legend_y = board.max.y + 10.0;
        let mut x = board.min.x + 2.0;
        for (label, color) in [
            ("Bound", Color32::from_rgb(38, 50, 70)),
            ("Pressed", ACCENT),
            ("Selected", Color32::from_rgb(78, 112, 166)),
            ("Search hit", WARN),
        ] {
            painter.circle_filled(Pos2::new(x + 3.0, legend_y), 3.0, color);
            let galley =
                painter.layout_no_wrap(label.to_string(), FontId::proportional(10.0), TEXT_DIM);
            let width = galley.size().x;
            painter.galley(
                Pos2::new(x + 10.0, legend_y - galley.size().y / 2.0),
                galley,
                TEXT_DIM,
            );
            x += 10.0 + width + 14.0;
        }
        clicked
    }

    fn reload_keymap(&mut self) {
        self.keymap = keymap::NiriKeymap::load_default();
        self.keymap_at = Instant::now();
    }
}

fn keymap_index(keymap: &keymap::NiriKeymap) -> HashMap<&'static str, Vec<&keymap::NiriBind>> {
    let mut index: HashMap<&'static str, Vec<&keymap::NiriBind>> = HashMap::new();
    for bind in keymap.effective() {
        if let Some(id) = bind.key_id {
            index.entry(id).or_default().push(bind);
        }
    }
    index
}

fn keymap_matches(bind: &keymap::NiriBind, query: &str) -> bool {
    bind.hotkey_text().to_ascii_lowercase().contains(query)
        || bind.action_text().to_ascii_lowercase().contains(query)
        || bind
            .title
            .as_deref()
            .is_some_and(|title| title.to_ascii_lowercase().contains(query))
}

/// Largest 1u key edge. Without the cap a wide dock stretches one key across
/// the panel and the board stops reading as a keyboard.
const KEYBOARD_MAX_KEY: f32 = 40.0;
/// Smallest 1u key edge that still reads as a keyboard in a short dock.
const KEYBOARD_MIN_KEY: f32 = 9.0;

/// Legend row kept under the board when the dock is tall enough to hold it.
fn keyboard_legend_height(available: Vec2) -> f32 {
    if available.y >= 150.0 { 18.0 } else { 0.0 }
}

/// Fit the 15x6 unit board into the dock, bounded by both dimensions so keys
/// stay square however the splitter is dragged.
fn keyboard_cell(available: Vec2) -> f32 {
    ((available.x - 12.0) / 15.0)
        .min((available.y - keyboard_legend_height(available) - 12.0) / 6.0)
        .clamp(KEYBOARD_MIN_KEY, KEYBOARD_MAX_KEY)
}

#[derive(Clone, Copy)]
struct ControllerColumns {
    label: f32,
    binding: f32,
    layers: f32,
}

/// Pin the controller table columns so the trailing trigger gauge cannot clip
/// at the panel edge when the dock is narrow.
fn controller_columns(list_w: f32) -> ControllerColumns {
    let label = 96.0_f32.min(list_w * 0.28);
    let remaining = (list_w - label - 22.0).max(160.0);
    let layers = remaining * 0.26;
    ControllerColumns {
        label,
        binding: remaining - layers,
        layers,
    }
}

fn keymap_list(ui: &mut Ui, binds: &[&keymap::NiriBind], height: f32) {
    let width = ui.available_width();
    let source_w = (width * 0.22).clamp(110.0, 170.0);
    let hotkey_w = (width * 0.30).clamp(120.0, 210.0);
    let action_w = (width - hotkey_w - source_w - 18.0).max(80.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        column_header(ui, "Hotkey", hotkey_w);
        column_header(ui, "Action", action_w);
        column_header(ui, "Source", source_w);
    });
    ui.add_space(2.0);
    ui.separator();
    ScrollArea::vertical()
        .id_salt("keymap-bind-list")
        .max_height((height - 32.0).max(40.0))
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.set_width(width);
            if binds.is_empty() {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("No matching binds")
                        .size(11.0)
                        .color(TEXT_DIM),
                );
                return;
            }
            for (index, bind) in binds.iter().enumerate() {
                keymap_row(ui, bind, index, hotkey_w, action_w, source_w);
            }
        });
}

/// Fixed-width, left-aligned cell. `Ui::add_sized` centers its widget, which
/// made table columns look ragged; this keeps every row on the same grid.
fn text_cell(ui: &mut Ui, width: f32, height: f32, text: RichText) -> egui::Response {
    ui.allocate_ui_with_layout(
        Vec2::new(width, height),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.set_min_height(height);
            ui.add(egui::Label::new(text).truncate())
        },
    )
    .inner
}

fn column_header(ui: &mut Ui, label: &str, width: f32) {
    text_cell(
        ui,
        width,
        18.0,
        RichText::new(label).size(10.0).color(TEXT_DIM),
    );
}

fn keymap_row(
    ui: &mut Ui,
    bind: &keymap::NiriBind,
    index: usize,
    hotkey_w: f32,
    action_w: f32,
    source_w: f32,
) {
    let fill = if index % 2 == 1 {
        CARD
    } else {
        Color32::TRANSPARENT
    };
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::symmetric(2, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let hotkey_color = if bind.duplicate {
                    ERROR
                } else if bind.shadowed {
                    TEXT_DIM
                } else {
                    TEXT
                };
                let hotkey = text_cell(
                    ui,
                    hotkey_w,
                    19.0,
                    RichText::new(bind.hotkey_text())
                        .monospace()
                        .size(11.0)
                        .color(hotkey_color),
                );
                let action = text_cell(
                    ui,
                    action_w,
                    19.0,
                    RichText::new(bind.action_text())
                        .monospace()
                        .size(11.0)
                        .color(if bind.shadowed { TEXT_DIM } else { TEXT_MUTED }),
                );
                let source = bind.file.file_name().and_then(|name| name.to_str());
                let detail = text_cell(
                    ui,
                    source_w,
                    19.0,
                    RichText::new(source.unwrap_or(""))
                        .size(10.5)
                        .color(if bind.duplicate {
                            ERROR
                        } else if bind.shadowed {
                            WARN
                        } else {
                            TEXT_DIM
                        }),
                );
                let mut hover = format!(
                    "{}\n{}\n{}\n{}",
                    bind.hotkey_text(),
                    bind.action_text(),
                    bind.title.as_deref().unwrap_or("(no overlay title)"),
                    bind.file.display()
                );
                if bind.duplicate {
                    hover.push_str("\nNiri refuses to start when one binds block repeats a key");
                } else if bind.shadowed {
                    hover.push_str("\nA later bind replaces this one");
                }
                hotkey.on_hover_text(&hover);
                action.on_hover_text(&hover);
                detail.on_hover_text(&hover);
            });
        });
}

fn controller_row(
    ui: &mut Ui,
    row: &ControllerRow,
    binding: Option<&str>,
    layers: &[String],
    label_w: f32,
    binding_w: f32,
    layer_w: f32,
) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let (dot, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
        ui.painter()
            .circle_filled(dot.center(), 2.8, if row.pressed { ACCENT } else { BORDER });
        text_cell(
            ui,
            label_w,
            19.0,
            RichText::new(&row.label)
                .size(11.0)
                .color(if row.pressed { TEXT } else { TEXT_MUTED }),
        );
        let text = match binding {
            Some(binding) => binding.to_string(),
            None => "unbound".to_string(),
        };
        let binding_response = text_cell(
            ui,
            binding_w,
            19.0,
            RichText::new(text)
                .monospace()
                .size(10.5)
                .color(if row.pressed { ACCENT } else { TEXT_DIM }),
        );
        if let Some(binding) = binding {
            binding_response.on_hover_text(binding);
        }
        let lane = if layers.is_empty() {
            "—".to_string()
        } else {
            layers.join(", ")
        };
        text_cell(
            ui,
            layer_w,
            19.0,
            RichText::new(lane)
                .size(10.0)
                .color(if layers.is_empty() { TEXT_DIM } else { WARN }),
        );
        if let Some(value) = row.trigger
            && value > 0.05
        {
            ui.label(
                RichText::new(format!("{:.0}%", value * 100.0))
                    .monospace()
                    .size(10.0)
                    .color(ACCENT),
            );
        }
    });
}

fn metric_chip(ui: &mut Ui, label: &str, value: impl ToString) {
    egui::Frame::new()
        .fill(FIELD_BG)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(4.0)
        .inner_margin(egui::Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(label).size(8.5).color(TEXT_DIM));
                ui.label(
                    RichText::new(value.to_string())
                        .monospace()
                        .size(9.0)
                        .color(TEXT),
                );
            });
        });
}

fn dock_tab_button(ui: &mut Ui, selected: bool, label: &str) -> egui::Response {
    let response = egui::Frame::new()
        .fill(if selected {
            Color32::from_rgb(36, 47, 64)
        } else {
            Color32::TRANSPARENT
        })
        .stroke(if selected {
            Stroke::new(1.0_f32, ACCENT)
        } else {
            Stroke::NONE
        })
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(9, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(10.5).color(if selected {
                Color32::WHITE
            } else {
                TEXT_MUTED
            }));
        })
        .response
        .interact(Sense::click());
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

fn on_off(value: Option<bool>) -> &'static str {
    if value.unwrap_or(false) { "on" } else { "off" }
}

fn pressed_key_ids(ctx: &egui::Context) -> BTreeSet<&'static str> {
    let mut pressed = BTreeSet::new();
    ctx.input(|input| {
        for key in &input.keys_down {
            if let Some(id) = egui_key_id(*key) {
                pressed.insert(id);
            }
        }
        let modifiers = input.modifiers;
        if modifiers.shift {
            pressed.insert("shiftleft");
        }
        if modifiers.ctrl {
            pressed.insert("ctrlleft");
        }
        if modifiers.alt {
            pressed.insert("altleft");
        }
        if modifiers.mac_cmd {
            pressed.insert("superleft");
        }
    });
    pressed
}

fn egui_key_id(key: egui::Key) -> Option<&'static str> {
    Some(match key {
        egui::Key::A => "a",
        egui::Key::B => "b",
        egui::Key::C => "c",
        egui::Key::D => "d",
        egui::Key::E => "e",
        egui::Key::F => "f",
        egui::Key::G => "g",
        egui::Key::H => "h",
        egui::Key::I => "i",
        egui::Key::J => "j",
        egui::Key::K => "k",
        egui::Key::L => "l",
        egui::Key::M => "m",
        egui::Key::N => "n",
        egui::Key::O => "o",
        egui::Key::P => "p",
        egui::Key::Q => "q",
        egui::Key::R => "r",
        egui::Key::S => "s",
        egui::Key::T => "t",
        egui::Key::U => "u",
        egui::Key::V => "v",
        egui::Key::W => "w",
        egui::Key::X => "x",
        egui::Key::Y => "y",
        egui::Key::Z => "z",
        egui::Key::Num0 => "0",
        egui::Key::Num1 => "1",
        egui::Key::Num2 => "2",
        egui::Key::Num3 => "3",
        egui::Key::Num4 => "4",
        egui::Key::Num5 => "5",
        egui::Key::Num6 => "6",
        egui::Key::Num7 => "7",
        egui::Key::Num8 => "8",
        egui::Key::Num9 => "9",
        egui::Key::ArrowUp => "up",
        egui::Key::ArrowDown => "down",
        egui::Key::ArrowLeft => "left",
        egui::Key::ArrowRight => "right",
        egui::Key::Escape => "escape",
        egui::Key::Tab => "tab",
        egui::Key::Backspace => "backspace",
        egui::Key::Enter => "return",
        egui::Key::Space => "space",
        egui::Key::Insert => "insert",
        egui::Key::Delete => "delete",
        egui::Key::Minus => "minus",
        egui::Key::Equals => "equal",
        egui::Key::Comma => "comma",
        egui::Key::Period => "period",
        egui::Key::Slash => "slash",
        egui::Key::Backslash => "backslash",
        egui::Key::Semicolon => "semicolon",
        egui::Key::Quote => "quote",
        egui::Key::Backtick => "grave",
        egui::Key::OpenBracket => "bracketleft",
        egui::Key::CloseBracket => "bracketright",
        egui::Key::F1 => "f1",
        egui::Key::F2 => "f2",
        egui::Key::F3 => "f3",
        egui::Key::F4 => "f4",
        egui::Key::F5 => "f5",
        egui::Key::F6 => "f6",
        egui::Key::F7 => "f7",
        egui::Key::F8 => "f8",
        egui::Key::F9 => "f9",
        egui::Key::F10 => "f10",
        egui::Key::F11 => "f11",
        egui::Key::F12 => "f12",
        _ => return None,
    })
}

impl eframe::App for StudioApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ensure_textures(ctx);
        self.command_bar(ctx);
        self.action_bar(ctx);
        if ctx.screen_rect().width() >= 700.0 {
            self.nav_rail(ctx);
        } else {
            self.nav_strip(ctx);
        }
        self.live_dock(ctx);
        self.content(ctx);
        self.confirm_modal(ctx);
        // The live poller updates a shared snapshot without waking egui.
        // Repaint while the dock is open so daemon focus changes, controller
        // presses and click feedback arrive even when the pointer is idle.
        if !self.dock_collapsed {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
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
            let title_response = ui.add(
                egui::Label::new(RichText::new(title).size(12.5).strong()).sense(Sense::hover()),
            );
            if let Some(hover) = i18n::hover_for(None, title) {
                title_response.on_hover_text(hover);
            }
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

/// Field row whose label and hover text come from the i18n catalog by
/// config path (see `i18n.rs`); `label` is only the fallback.
fn field_row_path(
    ui: &mut Ui,
    path: Option<&str>,
    label: &str,
    add: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let label = path.map_or(label, |path| i18n::label_for_path(path, label));
    let label_width = (ui.available_width() * 0.42).clamp(110.0, 190.0);
    ui.horizontal(|ui| {
        let label_response = ui.add_sized(
            Vec2::new(label_width, 22.0),
            egui::Label::new(RichText::new(label).size(12.0).color(TEXT_MUTED))
                .truncate()
                .sense(Sense::hover()),
        );
        if let Some(hover) = i18n::hover_for(path, label) {
            label_response.on_hover_text(hover);
        }
        add(ui)
    })
    .inner
}

/// Attach the catalog hover text for a `ui.*` key to a widget response.
fn tt(response: egui::Response, key: &str) -> egui::Response {
    match i18n::hover_for_key(key) {
        Some(hover) => response.on_hover_text(hover),
        None => response,
    }
}

fn field_label(ui: &mut Ui, label: &str) {
    let width = (ui.available_width() * 0.34).clamp(110.0, 180.0);
    let response = ui.add_sized(
        Vec2::new(width, 22.0),
        egui::Label::new(RichText::new(label).size(12.0).color(TEXT_MUTED))
            .truncate()
            .sense(Sense::hover()),
    );
    if let Some(hover) = i18n::hover_for(None, label) {
        response.on_hover_text(hover);
    }
}

fn input_width(ui: &Ui) -> f32 {
    (ui.available_width() - 2.0).clamp(64.0, 360.0)
}

/// Standard buttons that are not bound yet, so one physical button never
/// lands on two rows of the same map.
fn button_options(used: &[String]) -> Vec<(&'static str, &'static str)> {
    BUTTONS
        .iter()
        .copied()
        .filter(|(code, _)| !used.iter().any(|value| value == code))
        .collect()
}

/// Menu of unbound standard buttons. Returns the picked evdev name.
fn button_menu(ui: &mut Ui, title: &str, used: &[String]) -> Option<&'static str> {
    let options = button_options(used);
    ui.menu_button(title, |ui| {
        let mut picked = None;
        if options.is_empty() {
            ui.label(
                RichText::new("All standard buttons are bound")
                    .size(10.5)
                    .color(TEXT_MUTED),
            );
        }
        for (code, label) in &options {
            if ui
                .button(
                    RichText::new(format!("{code}   {label}"))
                        .monospace()
                        .size(11.0),
                )
                .clicked()
            {
                picked = Some(*code);
                ui.close();
            }
        }
        picked
    })
    .inner
    .flatten()
}

/// Soft curve from the selected control on the controller image to its row in
/// the binding lists, with a dot at each end.
fn paint_binding_link(ui: &Ui, from: Rect, to: Rect) {
    let clip = ui.clip_rect();
    if !clip.contains(from.center()) || !clip.contains(to.center()) || to.left() <= from.right() {
        return;
    }
    let start = Pos2::new(from.right() + 4.0, from.center().y);
    let end = Pos2::new(to.left() - 6.0, to.center().y);
    let reach = ((end.x - start.x) * 0.45).clamp(20.0, 90.0);
    let color = Color32::from_rgba_unmultiplied(116, 162, 255, 150);
    let painter = ui.painter();
    painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
        [
            start,
            Pos2::new(start.x + reach, start.y),
            Pos2::new(end.x - reach, end.y),
            end,
        ],
        false,
        Color32::TRANSPARENT,
        Stroke::new(1.5_f32, color),
    ));
    painter.circle_filled(start, 2.5, color);
    painter.circle_filled(end, 3.0, color);
}

fn mini_heading(ui: &mut Ui, text: &str) {
    let response = ui.add(
        egui::Label::new(RichText::new(text).size(10.5).strong().color(TEXT_DIM))
            .sense(Sense::hover()),
    );
    if let Some(hover) = i18n::hover_for(None, text) {
        response.on_hover_text(hover);
    }
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

fn nav_item(ui: &mut Ui, selected: bool, label: &str) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 34.0), Sense::click());
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
        Pos2::new(rect.min.x + 13.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(13.5),
        if selected {
            Color32::from_rgb(236, 241, 250)
        } else {
            Color32::from_rgb(190, 198, 211)
        },
    );
    if response.hovered() {
        response.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        response
    }
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

#[derive(Clone, Copy, Debug, PartialEq)]
struct CurvePoint {
    pull: f32,
    output: f32,
}

fn trigger_shape(value: f32, deadzone: f32, curve: f32, points: &[CurvePoint]) -> f32 {
    let value = value.clamp(0.0, 1.0);
    if value <= deadzone {
        return 0.0;
    }
    if points.is_empty() {
        let normalized = ((value - deadzone) / (1.0 - deadzone).max(0.001)).clamp(0.0, 1.0);
        return normalized.powf(curve.max(0.05));
    }
    let mut previous_pull = deadzone;
    let mut previous_output = 0.0_f32;
    for point in points {
        if value <= point.pull {
            let span = (point.pull - previous_pull).max(0.001);
            let t = ((value - previous_pull) / span).clamp(0.0, 1.0);
            return previous_output + (point.output - previous_output) * t;
        }
        previous_pull = point.pull;
        previous_output = point.output;
    }
    let span = (1.0 - previous_pull).max(0.001);
    previous_output + (1.0 - previous_output) * ((value - previous_pull) / span).clamp(0.0, 1.0)
}

fn trigger_multiplier(
    decel_floor: f32,
    boost_cap: f32,
    lt: f32,
    rt: f32,
    deadzone: f32,
    curve: f32,
    points: &[CurvePoint],
) -> f32 {
    decel_floor.powf(trigger_shape(lt, deadzone, curve, points))
        * boost_cap.powf(trigger_shape(rt, deadzone, curve, points))
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

fn main() -> Result<()> {
    if env::args().any(|arg| arg == "--print-strings") {
        print!("{}", i18n::en_template_json());
        return Ok(());
    }
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
                    "usage: niri-xmb-config-studio [--config PATH] [--niri-xmb PATH] [--print-strings]\n\
                     Native TOML editor. Validation and save use the niri-xmb binary.\n\
                     --print-strings prints the English display-text template (locales/en.json) and exits."
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
    fn live_gamepad_view_aggregates_devices_and_dpad_hats() {
        let status = json!({
            "gamepad": {
                "live": [
                    {
                        "path": "/dev/input/event1",
                        "buttons": ["BTN_SOUTH"],
                        "axes": {"ABS_Z": 0.4, "ABS_HAT0X": -1.0}
                    },
                    {
                        "path": "/dev/input/event2",
                        "buttons": ["BTN_THUMBL"],
                        "axes": {"ABS_Z": 0.8, "ABS_HAT0Y": 1.0}
                    }
                ]
            }
        });
        let view = GamepadLiveView::from_status(&status);
        assert!(view.button("BTN_SOUTH"));
        assert!(view.button("BTN_THUMBL"));
        assert_eq!(view.trigger("ABS_Z", "BTN_TL2"), 0.8);
        assert!(view.dpad("left"));
        assert!(view.dpad("down"));
        assert!(!view.dpad("up"));
        assert!(!view.dpad("right"));
    }

    #[test]
    fn live_gamepad_view_is_empty_without_the_live_field() {
        let view = GamepadLiveView::from_status(&json!({"gamepad": {"devices": 0}}));
        assert!(view.buttons.is_empty());
        assert!(view.axes.is_empty());
    }

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
        assert!(trigger_shape(0.0, 0.08, 1.0, &[]).abs() < 1e-6);
        assert!(trigger_shape(0.08, 0.08, 1.0, &[]).abs() < 1e-6);
        assert!((trigger_shape(1.0, 0.08, 1.0, &[]) - 1.0).abs() < 1e-6);
        assert!((trigger_shape(0.5, 0.0, 1.0, &[]) - 0.5).abs() < 1e-6);
        assert!(trigger_shape(0.5, 0.0, 2.0, &[]) < 0.5);
        assert!(
            (trigger_shape(0.5, 0.0, 0.0, &[]) - trigger_shape(0.5, 0.0, 0.05, &[])).abs() < 1e-6
        );
    }

    #[test]
    fn trigger_points_interpolate_and_fall_back() {
        let points = [
            CurvePoint {
                pull: 0.25,
                output: 0.10,
            },
            CurvePoint {
                pull: 0.50,
                output: 0.30,
            },
            CurvePoint {
                pull: 0.75,
                output: 0.70,
            },
        ];
        assert!((trigger_shape(0.08, 0.08, 1.0, &points) - 0.0).abs() < 1e-6);
        assert!((trigger_shape(0.25, 0.08, 1.0, &points) - 0.10).abs() < 1e-6);
        assert!((trigger_shape(0.50, 0.08, 1.0, &points) - 0.30).abs() < 1e-6);
        assert!((trigger_shape(0.75, 0.08, 1.0, &points) - 0.70).abs() < 1e-6);
        assert!((trigger_shape(1.0, 0.08, 1.0, &points) - 1.0).abs() < 1e-6);
        assert!((trigger_shape(0.50, 0.08, 3.0, &[]) - (0.42_f32 / 0.92).powf(3.0)).abs() < 1e-6);
    }

    #[test]
    fn trigger_multiplier_matches_daemon_targets() {
        let multiplier = |lt, rt| trigger_multiplier(0.3, 2.5, lt, rt, 0.08, 1.0, &[]);
        assert!((multiplier(0.0, 0.0) - 1.0).abs() < 1e-5);
        assert!((multiplier(1.0, 0.0) - 0.3).abs() < 1e-5);
        assert!((multiplier(0.0, 1.0) - 2.5).abs() < 1e-5);
        assert!((multiplier(1.0, 1.0) - 0.75).abs() < 1e-5);
        assert!((multiplier(0.08, 0.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn binding_dropdown_lists_standard_buttons_only() {
        let used = vec!["BTN_SOUTH".to_string(), "0x130".to_string()];
        let options = button_options(&used);
        assert!(!options.iter().any(|(code, _)| *code == "BTN_SOUTH"));
        assert!(options.iter().any(|(code, _)| *code == "BTN_EAST"));

        let mut codes = BUTTONS.iter().map(|(code, _)| *code).collect::<Vec<_>>();
        let count = codes.len();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), count, "dropdown codes must be unique");
    }

    #[test]
    fn binding_dropdown_codes_pass_config_validation() {
        let mut source = String::from("[gamepad.navi.bindings]\n");
        for (code, _) in BUTTONS {
            source.push_str(&format!("{code} = \"pointer.left\"\n"));
        }
        let config: Config = toml::from_str(&source).unwrap();
        config.validate().unwrap();
        use std::str::FromStr;
        for (code, _) in BUTTONS {
            assert!(
                evdev::KeyCode::from_str(code).is_ok(),
                "{code} must be a known evdev button"
            );
        }
    }

    #[test]
    fn keyboard_cell_keeps_key_proportions() {
        // Wide dock: the board must not stretch one key across the panel.
        let wide = keyboard_cell(Vec2::new(1800.0, 400.0));
        assert_eq!(wide, KEYBOARD_MAX_KEY);

        // Short dock: height is the binding constraint, so keys shrink with it.
        let tall = keyboard_cell(Vec2::new(1800.0, 300.0));
        let short = keyboard_cell(Vec2::new(1800.0, 120.0));
        assert!(short < tall);
        assert!(6.0 * short <= 120.0);
    }

    #[test]
    fn keyboard_cell_fits_the_dock_budget() {
        for height in [90.0_f32, 120.0, 215.0, 360.0, 700.0] {
            let available = Vec2::new(1200.0, height);
            let cell = keyboard_cell(available);
            let board = Vec2::new(
                15.0 * cell + 10.0,
                6.0 * cell + 10.0 + keyboard_legend_height(available),
            );
            assert!(
                board.x <= available.x,
                "{height}: board wider than the dock"
            );
            assert!(
                board.y <= available.y,
                "{height}: board taller than the dock"
            );
        }
        // Width-bound docks still respect the column budget.
        let narrow = keyboard_cell(Vec2::new(240.0, 400.0));
        assert!(15.0 * narrow + 10.0 <= 240.0);
    }

    #[test]
    fn controller_columns_stay_inside_the_table() {
        for width in [240.0_f32, 320.0, 480.0, 760.0, 1200.0] {
            let columns = controller_columns(width);
            let used = columns.label + columns.binding + columns.layers;
            assert!(used <= width, "{width}: columns overflow the table");
            assert!(
                columns.binding >= 110.0,
                "{width}: binding column collapsed"
            );
            assert!(columns.layers > 0.0 && columns.label > 0.0);
        }
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
