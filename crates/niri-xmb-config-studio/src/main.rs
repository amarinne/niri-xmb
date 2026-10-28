//! Native editor for niri-xmb TOML.
//!
//! Left navigation selects a section, the command bar loads, validates and
//! saves, and the popup and keyboard pages show live renderer and Niri keybind
//! state alongside their editors. The controller editor map also reflects live input when available. The
//! editor loads the real schema through
//! `niri-xmb config-json`, writes only changed paths through `toml_edit`,
//! validates with `config-check`, and saves atomically. It does not talk to the
//! daemon except through the existing `restart` command after an explicit Save
//! & Restart.

mod i18n;
mod keymap;
mod single_instance;
mod validation;

use anyhow::{Context, Result};
use eframe::egui;
use egui::{
    Align, Align2, Color32, ColorImage, ComboBox, CursorIcon, FontId, Key, Layout, Modifiers, Pos2,
    Rect, RichText, ScrollArea, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2,
};
use niri_xmb::config::{Config, ConfigLengthBounds, config_length_bounds};
use niri_xmb::preview::{
    PopupPreview, PreviewFrame, PreviewSelection, PreviewTarget, snapshot_from_json,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write as _};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEFAULT_CONFIG: &str = "~/.config/niri-xmb/config.toml";
const XBOX_IMAGE: &[u8] = include_bytes!("../../../assets/controller-render.png");
const DS4_IMAGE: &[u8] = include_bytes!("../../../assets/dualshock4-render.png");
const CONTROLLER_XBOX_SVG: &str = include_str!("../../../assets/controller-xbox.svg");
const XBOX_ASPECT: f32 = 403.0 / 441.0;
const DS4_ASPECT: f32 = 403.0 / 441.0;

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
    "pointer.back",
    "pointer.forward",
    "pointer.move-up",
    "pointer.move-down",
    "pointer.move-left",
    "pointer.move-right",
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
    "key.esc",
    "key.enter",
    "key.space",
    "key.tab",
    "key.leftctrl",
    "key.leftshift",
    "key.leftalt",
    "key.leftmeta",
    "key.backspace",
    "key.delete",
    "key.up",
    "key.down",
    "key.left",
    "key.right",
    "key.f1",
    "key.f2",
    "key.f3",
    "key.f4",
    "key.f5",
    "key.f6",
    "key.f7",
    "key.f8",
    "key.f9",
    "key.f10",
    "key.f11",
    "key.f12",
    "key.a",
    "key.b",
    "key.c",
    "key.d",
    "key.e",
    "key.f",
    "key.g",
    "key.h",
    "key.i",
    "key.j",
    "key.k",
    "key.l",
    "key.m",
    "key.n",
    "key.o",
    "key.p",
    "key.q",
    "key.r",
    "key.s",
    "key.t",
    "key.u",
    "key.v",
    "key.w",
    "key.x",
    "key.y",
    "key.z",
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
    ("style.panel_radius", "Panel radius", 0.0, 64.0, 0.5),
    ("style.radius", "Small radius", 0.0, 64.0, 0.5),
    ("style.border_width", "Border width", 0.0, 8.0, 0.1),
    ("style.dim_alpha", "Dim alpha", 0.0, 1.0, 0.01),
    ("style.panel_padding", "Panel padding", 0.0, 64.0, 0.5),
    ("style.gap", "Gap", 0.0, 64.0, 0.5),
    ("style.canvas_gap", "Canvas gap", 0.0, 64.0, 0.5),
    ("style.column_gap", "Column gap", 0.0, 64.0, 0.5),
    ("style.stack_gap", "Stack gap", 0.0, 64.0, 0.5),
    ("style.row_gap", "Row gap", 0.0, 64.0, 0.5),
    ("style.shadow_offset", "Shadow offset", -32.0, 32.0, 0.5),
    ("style.shadow_spread", "Shadow spread", 0.0, 32.0, 0.5),
    ("style.empty_dot_radius", "Empty dot", 0.0, 16.0, 0.5),
    ("style.label_font_size", "Label size", 1.0, 32.0, 0.5),
    ("style.title_font_size", "Title size", 1.0, 32.0, 0.5),
    ("style.urgent_ring_width", "Urgent ring", 0.0, 8.0, 0.1),
    (
        "style.urgent_ring_padding",
        "Urgent padding",
        0.0,
        16.0,
        0.1,
    ),
    ("style.urgent_glow_spread", "Urgent spread", 0.0, 32.0, 0.1),
    ("style.urgent_glow_alpha", "Urgent alpha", 0.0, 1.0, 0.01),
    ("style.urgent_badge_radius", "Urgent badge", 0.0, 16.0, 0.1),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum HotspotShape {
    Circle,
    Rect { corner_radius: f32 },
    DpadUp,
    DpadDown,
    DpadLeft,
    DpadRight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LengthUnit {
    Pixels,
    Percent,
}

impl LengthUnit {
    fn label(self) -> &'static str {
        match self {
            Self::Pixels => "px",
            Self::Percent => "%",
        }
    }
}

fn parse_length_value(value: &Value) -> (LengthUnit, String, Option<f64>) {
    match value {
        Value::String(text) if text.trim_end().ends_with('%') => {
            let number = text.trim_end().trim_end_matches('%').trim().to_string();
            (LengthUnit::Percent, number.clone(), number.parse().ok())
        }
        Value::String(text) => {
            let number = text.clone();
            (
                LengthUnit::Pixels,
                number.clone(),
                number.trim().parse().ok(),
            )
        }
        Value::Number(number) => {
            let number = number.to_string();
            (LengthUnit::Pixels, number.clone(), number.parse().ok())
        }
        _ => (LengthUnit::Pixels, String::new(), None),
    }
}

fn format_length_value(unit: LengthUnit, number: &str) -> Value {
    match unit {
        LengthUnit::Pixels => number
            .trim()
            .parse::<f64>()
            .map_or_else(|_| Value::String(number.to_string()), |value| json!(value)),
        LengthUnit::Percent => Value::String(format!("{}%", number.trim())),
    }
}

fn toggle_length_value(
    unit: LengthUnit,
    value: f64,
    reference_extent: Option<f64>,
    bounds: ConfigLengthBounds,
) -> (LengthUnit, f64) {
    let next = match unit {
        LengthUnit::Pixels => LengthUnit::Percent,
        LengthUnit::Percent => LengthUnit::Pixels,
    };
    let converted = reference_extent
        .filter(|extent| extent.is_finite() && *extent > 0.0)
        .map_or(value, |extent| match (unit, next) {
            (LengthUnit::Pixels, LengthUnit::Percent) => value / extent * 100.0,
            (LengthUnit::Percent, LengthUnit::Pixels) => value * extent / 100.0,
            _ => value,
        });
    let (min, max) = match next {
        LengthUnit::Pixels => (bounds.min_pixels as f64, bounds.max_pixels as f64),
        LengthUnit::Percent => (bounds.min_percent as f64, bounds.max_percent as f64),
    };
    let value = if reference_extent.is_some() {
        converted.clamp(min, max)
    } else {
        converted
    };
    (next, value)
}

fn length_slider_bounds(bounds: ConfigLengthBounds, unit: LengthUnit) -> (f64, f64) {
    let (min, max) = match unit {
        LengthUnit::Pixels => (bounds.min_pixels as f64, bounds.max_pixels as f64),
        LengthUnit::Percent => (bounds.min_percent as f64, bounds.max_percent as f64),
    };
    // Keep the common range easy to scan while retaining the complete daemon
    // range in the editable DragValue.
    let min = if min < 0.0 { min.max(-1200.0) } else { min };
    let max = if max > 1200.0 { max.min(1200.0) } else { max };
    (min.min(max), max.max(min))
}

struct Hotspot {
    control: &'static str,
    code: Option<&'static str>,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    shape: HotspotShape,
    label: bool,
}

impl Hotspot {
    fn is_pressed(&self, live: &GamepadLiveView) -> bool {
        match self.control {
            "LT" | "L2" => live.trigger("ABS_Z", "BTN_TL2") > 0.05,
            "RT" | "R2" => live.trigger("ABS_RZ", "BTN_TR2") > 0.05,
            "L STICK" | "L3" => live.button("BTN_THUMBL"),
            "R STICK" | "R3" => live.button("BTN_THUMBR"),
            "D-pad up" | "D-pad Up" => live.dpad("up"),
            "D-pad down" | "D-pad Down" => live.dpad("down"),
            "D-pad left" | "D-pad Left" => live.dpad("left"),
            "D-pad right" | "D-pad Right" => live.dpad("right"),
            _ => self.code.is_some_and(|code| live.button(code)),
        }
    }
}

const XBOX_HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "LT",
        code: None,
        x: 0.3141,
        y: 0.0583,
        w: 0.0680,
        h: 0.0998,
        shape: HotspotShape::Rect { corner_radius: 6.0 },
        label: false,
    },
    Hotspot {
        control: "RT",
        code: None,
        x: 0.6882,
        y: 0.0583,
        w: 0.0680,
        h: 0.0998,
        shape: HotspotShape::Rect { corner_radius: 6.0 },
        label: false,
    },
    Hotspot {
        control: "LB",
        code: Some("BTN_TL"),
        x: 0.2993,
        y: 0.1712,
        w: 0.0975,
        h: 0.0385,
        shape: HotspotShape::Rect { corner_radius: 7.0 },
        label: false,
    },
    Hotspot {
        control: "RB",
        code: Some("BTN_TR"),
        x: 0.7029,
        y: 0.1712,
        w: 0.0975,
        h: 0.0385,
        shape: HotspotShape::Rect { corner_radius: 7.0 },
        label: false,
    },
    Hotspot {
        control: "L STICK",
        code: None,
        x: 0.2562,
        y: 0.3970,
        w: 0.1315,
        h: 0.1315,
        shape: HotspotShape::Circle,
        label: true,
    },
    Hotspot {
        control: "R STICK",
        code: None,
        x: 0.6304,
        y: 0.5906,
        w: 0.1315,
        h: 0.1315,
        shape: HotspotShape::Circle,
        label: true,
    },
    Hotspot {
        control: "D-pad up",
        code: Some("BTN_DPAD_UP"),
        x: 0.3764,
        y: 0.5459,
        w: 0.0635,
        h: 0.0499,
        shape: HotspotShape::DpadUp,
        label: false,
    },
    Hotspot {
        control: "D-pad down",
        code: Some("BTN_DPAD_DOWN"),
        x: 0.3764,
        y: 0.6352,
        w: 0.0635,
        h: 0.0499,
        shape: HotspotShape::DpadDown,
        label: false,
    },
    Hotspot {
        control: "D-pad left",
        code: Some("BTN_DPAD_LEFT"),
        x: 0.3356,
        y: 0.5906,
        w: 0.0499,
        h: 0.0635,
        shape: HotspotShape::DpadLeft,
        label: false,
    },
    Hotspot {
        control: "D-pad right",
        code: Some("BTN_DPAD_RIGHT"),
        x: 0.4172,
        y: 0.5906,
        w: 0.0499,
        h: 0.0635,
        shape: HotspotShape::DpadRight,
        label: false,
    },
    Hotspot {
        control: "Y",
        code: Some("BTN_NORTH"),
        x: 0.7460,
        y: 0.3424,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "X",
        code: Some("BTN_WEST"),
        x: 0.6961,
        y: 0.3970,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "B",
        code: Some("BTN_EAST"),
        x: 0.7959,
        y: 0.3970,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "A",
        code: Some("BTN_SOUTH"),
        x: 0.7460,
        y: 0.4516,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "SELECT",
        code: Some("BTN_SELECT"),
        x: 0.4195,
        y: 0.4020,
        w: 0.0454,
        h: 0.0454,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "START",
        code: Some("BTN_START"),
        x: 0.5873,
        y: 0.4020,
        w: 0.0454,
        h: 0.0454,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "GUIDE",
        code: Some("BTN_MODE"),
        x: 0.5000,
        y: 0.4020,
        w: 0.0726,
        h: 0.0726,
        shape: HotspotShape::Circle,
        label: false,
    },
];

const DS4_HOTSPOTS: &[Hotspot] = &[
    Hotspot {
        control: "Touchpad",
        code: None,
        x: 0.5000,
        y: 0.3127,
        w: 0.2426,
        h: 0.1315,
        shape: HotspotShape::Rect { corner_radius: 6.0 },
        label: false,
    },
    Hotspot {
        control: "L2",
        code: Some("BTN_TL2"),
        x: 0.3141,
        y: 0.0583,
        w: 0.0680,
        h: 0.0998,
        shape: HotspotShape::Rect { corner_radius: 6.0 },
        label: false,
    },
    Hotspot {
        control: "R2",
        code: Some("BTN_TR2"),
        x: 0.6882,
        y: 0.0583,
        w: 0.0680,
        h: 0.0998,
        shape: HotspotShape::Rect { corner_radius: 6.0 },
        label: false,
    },
    Hotspot {
        control: "L1",
        code: Some("BTN_TL"),
        x: 0.2993,
        y: 0.1712,
        w: 0.0975,
        h: 0.0385,
        shape: HotspotShape::Rect { corner_radius: 7.0 },
        label: false,
    },
    Hotspot {
        control: "R1",
        code: Some("BTN_TR"),
        x: 0.7029,
        y: 0.1712,
        w: 0.0975,
        h: 0.0385,
        shape: HotspotShape::Rect { corner_radius: 7.0 },
        label: false,
    },
    Hotspot {
        control: "Triangle",
        code: Some("BTN_NORTH"),
        x: 0.7460,
        y: 0.3424,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "Circle",
        code: Some("BTN_EAST"),
        x: 0.7959,
        y: 0.3970,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "Cross",
        code: Some("BTN_SOUTH"),
        x: 0.7460,
        y: 0.4516,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "Square",
        code: Some("BTN_WEST"),
        x: 0.6961,
        y: 0.3970,
        w: 0.0590,
        h: 0.0590,
        shape: HotspotShape::Circle,
        label: false,
    },
    Hotspot {
        control: "D-pad up",
        code: Some("BTN_DPAD_UP"),
        x: 0.2562,
        y: 0.3524,
        w: 0.0635,
        h: 0.0499,
        shape: HotspotShape::DpadUp,
        label: false,
    },
    Hotspot {
        control: "D-pad down",
        code: Some("BTN_DPAD_DOWN"),
        x: 0.2562,
        y: 0.4417,
        w: 0.0635,
        h: 0.0499,
        shape: HotspotShape::DpadDown,
        label: false,
    },
    Hotspot {
        control: "D-pad left",
        code: Some("BTN_DPAD_LEFT"),
        x: 0.2154,
        y: 0.3970,
        w: 0.0499,
        h: 0.0635,
        shape: HotspotShape::DpadLeft,
        label: false,
    },
    Hotspot {
        control: "D-pad right",
        code: Some("BTN_DPAD_RIGHT"),
        x: 0.2971,
        y: 0.3970,
        w: 0.0499,
        h: 0.0635,
        shape: HotspotShape::DpadRight,
        label: false,
    },
    Hotspot {
        control: "L3",
        code: Some("BTN_THUMBL"),
        x: 0.3764,
        y: 0.5906,
        w: 0.1315,
        h: 0.1315,
        shape: HotspotShape::Circle,
        label: true,
    },
    Hotspot {
        control: "R3",
        code: Some("BTN_THUMBR"),
        x: 0.6304,
        y: 0.5906,
        w: 0.1315,
        h: 0.1315,
        shape: HotspotShape::Circle,
        label: true,
    },
    Hotspot {
        control: "Share",
        code: Some("BTN_SELECT"),
        x: 0.3560,
        y: 0.2903,
        w: 0.0317,
        h: 0.0544,
        shape: HotspotShape::Rect { corner_radius: 4.0 },
        label: false,
    },
    Hotspot {
        control: "Options",
        code: Some("BTN_START"),
        x: 0.6440,
        y: 0.2903,
        w: 0.0317,
        h: 0.0544,
        shape: HotspotShape::Rect { corner_radius: 4.0 },
        label: false,
    },
    Hotspot {
        control: "PS",
        code: Some("BTN_MODE"),
        x: 0.5000,
        y: 0.5906,
        w: 0.0635,
        h: 0.0635,
        shape: HotspotShape::Circle,
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
        None
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
            (_, "D-PAD") => Some("D-pad up"),
            (_, "D-pad up" | "D-pad Up") => Some("D-pad up"),
            (_, "D-pad down" | "D-pad Down") => Some("D-pad down"),
            (_, "D-pad left" | "D-pad Left") => Some("D-pad left"),
            (_, "D-pad right" | "D-pad Right") => Some("D-pad right"),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Controller,
    Popup,
    Keyboard,
}

impl Section {
    const ALL: [(Section, &'static str); 3] = [
        (Self::Controller, "Controller"),
        (Self::Popup, "Popup"),
        (Self::Keyboard, "Keyboard"),
    ];

    fn title(self) -> &'static str {
        match self {
            Self::Controller => "Controller",
            Self::Popup => "Popup",
            Self::Keyboard => "Keyboard",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Controller => "Device, pointer, bindings, layers",
            Self::Popup => "Panel, behavior, peek, animation, theme, colors, bars",
            Self::Keyboard => "Niri keybinds and live input",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum ControllerTab {
    #[default]
    Bindings,
    Curves,
    Diagnostics,
    Device,
}

impl ControllerTab {
    fn label(self) -> &'static str {
        match self {
            Self::Bindings => "Bindings & Layers",
            Self::Curves => "Analog & Curves",
            Self::Diagnostics => "Diagnostics",
            Self::Device => "Device Settings",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Bindings => "⌨",
            Self::Curves => "∿",
            Self::Diagnostics => "⏱",
            Self::Device => "⚙",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum PopupTab {
    #[default]
    PanelPlacements,
    BehaviorPeekAnimation,
    Theme,
    Bars,
}

impl PopupTab {
    const ALL: [Self; 4] = [
        Self::PanelPlacements,
        Self::BehaviorPeekAnimation,
        Self::Theme,
        Self::Bars,
    ];

    const fn label(self) -> &'static str {
        match self {
            Self::PanelPlacements => "Panel & Placements",
            Self::BehaviorPeekAnimation => "Behavior, Peek & Animation",
            Self::Theme => "Theme & Colors",
            Self::Bars => "Bars",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ControlSource {
    LeftStick,
    RightStick,
    Dpad,
    Other,
}

impl ControlSource {
    fn label(self) -> &'static str {
        match self {
            Self::LeftStick => "Left Stick",
            Self::RightStick => "Right Stick",
            Self::Dpad => "D-pad",
            Self::Other => "Other",
        }
    }
}

#[derive(Clone, Default)]
struct LiveState {
    status: Value,
    snapshot: Value,
    error: String,
    updated_at: Option<Instant>,
    /// When the newest pushed gamepad frame arrived (local clock).
    gamepad_at: Option<Instant>,
    /// Smoothed pushed-gamepad frame rate, measured on arrival.
    gamepad_hz: f32,
    gamepad_frames: u64,
    /// Whether the subscribe socket is currently open. Idle desktops push
    /// no frames, so this — not frame age — is what proves the stream is
    /// alive. Set by the stream thread on connect/disconnect.
    stream_alive: bool,
    /// How many times the stream thread has re-entered its connect loop.
    reconnects: u64,
}

/// Controller telemetry as the studio actually knows it.
///
/// An idle desktop pushes no gamepad frames, so a connected controller with
/// no recent input is `Idle`, not stale. Only a dead stream with a seeded
/// status is `Reconnecting`, and only a dead stream with no status at all
/// is `Offline`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum TelemetryState {
    Offline,
    Reconnecting,
    NoController,
    Idle,
    Live,
}

/// Classify controller telemetry without timers or polling: the stream
/// thread owns `stream_alive`, the daemon owns the device list, and frame
/// age only separates recent input (`Live`) from a connected-but-quiet
/// controller (`Idle`).
#[allow(dead_code)]
fn telemetry_state(
    status_seeded: bool,
    stream_alive: bool,
    connected: usize,
    gamepad_age_ms: Option<u128>,
    has_input_channels: bool,
) -> TelemetryState {
    if !status_seeded && !stream_alive {
        return TelemetryState::Offline;
    }
    if !stream_alive {
        return TelemetryState::Reconnecting;
    }
    if connected == 0 {
        return TelemetryState::NoController;
    }
    if has_input_channels && gamepad_age_ms.is_some_and(|age| age < 750) {
        return TelemetryState::Live;
    }
    TelemetryState::Idle
}

/// One request/response round-trip on the control socket (no subprocess).
fn socket_request(socket: &Path, request: &str) -> Option<Value> {
    let mut stream = UnixStream::connect(socket).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .ok()?;
    stream.write_all(request.as_bytes()).ok()?;
    stream.shutdown(std::net::Shutdown::Write).ok()?;
    let mut text = String::new();
    stream.read_to_string(&mut text).ok()?;
    serde_json::from_str(&text).ok()
}

fn control_socket_path(status: Option<&Value>) -> PathBuf {
    if let Some(path) = status.and_then(|status| status["socket"].as_str()) {
        return PathBuf::from(path);
    }
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/user/1000"));
    runtime.join("niri-xmb.sock")
}

/// Live data arrives on the daemon's push stream, never on a timer.
///
/// One `status` request per (re)connect seeds the non-gamepad fields and
/// locates the socket; after that the thread blocks reading NDJSON frames
/// and wakes egui only when a frame actually arrives. Gamepad frames are
/// coalesced by the daemon to ~60 Hz, so stick motion is smooth while an
/// idle desktop costs no wakeups at all. The 250 ms read timeout exists
/// only so shutdown can be observed while the stream is idle.
fn spawn_live(
    ctx: egui::Context,
    niri_xmb: PathBuf,
    live: Arc<Mutex<LiveState>>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("niri-xmb-studio-live".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Seed once per connection: status (socket/mirror paths,
                // niri health) plus the mirrored snapshot file.
                let status = run_niri(&niri_xmb, &["status"])
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                let socket = control_socket_path(status.as_ref());
                let snapshot = status
                    .as_ref()
                    .and_then(|status| status["snapshot_mirror"].as_str())
                    .map(PathBuf::from)
                    .and_then(|path| fs::read_to_string(path).ok())
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .or_else(|| socket_request(&socket, "snapshot"));
                if let Ok(mut state) = live.lock() {
                    if let Some(status) = status {
                        state.status = status;
                        state.error.clear();
                        state.updated_at = Some(Instant::now());
                    }
                    if let Some(snapshot) = snapshot {
                        state.snapshot = snapshot;
                    }
                    if state.status.is_null() && state.snapshot.is_null() {
                        state.error = "daemon status unavailable".to_string();
                    }
                }
                ctx.request_repaint();

                if stream_live(&ctx, &socket, &live, &stop) {
                    return; // stop requested
                }
                // Stream ended (daemon restart or old daemon): record the
                // disconnect so the UI reports reconnecting instead of
                // stale telemetry, then reseed and reconnect.
                if let Ok(mut state) = live.lock() {
                    state.stream_alive = false;
                    state.reconnects += 1;
                }
                ctx.request_repaint();
                for _ in 0..5 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
        .expect("spawn live studio stream")
}

/// Block on the subscribe stream until it ends or `stop` is set.
/// Returns true when the caller should exit instead of reconnecting.
fn stream_live(
    ctx: &egui::Context,
    socket: &Path,
    live: &Arc<Mutex<LiveState>>,
    stop: &Arc<AtomicBool>,
) -> bool {
    let Ok(stream) = UnixStream::connect(socket) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(250)));
    let Ok(mut writer) = stream.try_clone() else {
        return false;
    };
    if writer.write_all(b"subscribe 1\n").is_err() {
        return false;
    }
    // The socket is open and the subscribe request is sent: an idle desktop
    // pushes no frames, but the stream is alive from here until EOF/error.
    if let Ok(mut state) = live.lock() {
        state.stream_alive = true;
        state.error.clear();
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut last_gamepad: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => return false,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(_) => return false,
        }
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let now = Instant::now();
        if let Ok(mut state) = live.lock() {
            match frame["type"].as_str() {
                Some("hello") => {
                    state.stream_alive = true;
                    state.error.clear();
                    state.updated_at = Some(now);
                }
                Some("snapshot") => {
                    state.snapshot = frame["snapshot"].clone();
                    state.error.clear();
                    state.updated_at = Some(now);
                }
                Some("gamepad") => {
                    if !state.status.is_object() {
                        state.status = json!({});
                    }
                    state.status["gamepad"] = frame["gamepad"].clone();
                    if let Some(previous) = last_gamepad {
                        let dt = now.duration_since(previous).as_secs_f32();
                        if dt > 0.0 {
                            let hz = (1.0 / dt).min(240.0);
                            state.gamepad_hz = if state.gamepad_hz <= 0.0 {
                                hz
                            } else {
                                state.gamepad_hz * 0.75 + hz * 0.25
                            };
                        }
                    }
                    last_gamepad = Some(now);
                    state.gamepad_at = Some(now);
                    state.gamepad_frames += 1;
                    state.error.clear();
                    state.updated_at = Some(now);
                }
                Some("error") => {
                    state.error = frame["message"]
                        .as_str()
                        .unwrap_or("daemon stream error")
                        .to_string();
                }
                _ => {}
            }
        }
        ctx.request_repaint();
    }
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
    history_undo: Vec<Value>,
    history_redo: Vec<Value>,
    history_cursor: Value,
    sha256: String,
    section: Section,
    show_changes: bool,
    show_candidate: bool,
    template: Template,
    selected: &'static str,
    context: String,
    controller_tab: ControllerTab,
    popup_tab: PopupTab,
    pointer_source: Option<ControlSource>,
    scroll_source: Option<ControlSource>,
    error: String,
    candidate: String,
    hot_save: bool,
    /// The live telemetry disclosure is user-controlled and must not change
    /// when pushed controller frames or window resizing cause a redraw.
    #[allow(dead_code)]
    controller_telemetry_expanded: bool,
    /// Pointer and trigger tuning disclosures, user-owned like telemetry so
    /// redraws never collapse or expand them behind the user's back.
    controller_pointer_expanded: bool,
    controller_trigger_expanded: bool,
    #[allow(dead_code)]
    display_pointer_multiplier: f32,
    #[allow(dead_code)]
    display_scroll_multiplier: f32,
    #[allow(dead_code)]
    display_multiplier_at: Instant,
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
    /// Pinned previews render in a fixed top panel that never scrolls away.
    preview_pinned: bool,
    keymap: keymap::NiriKeymap,
    keymap_at: Instant,
    keymap_search: String,
    keymap_selected: Option<String>,
    field_errors: Vec<validation::FieldError>,
    validated_model: Value,
    validated_path: String,
}

#[derive(Clone, Debug)]
struct DpadVectorArtwork {
    up_polygon: Vec<Pos2>,
    up_arrow: [Pos2; 3],
    right_polygon: Vec<Pos2>,
    right_arrow: [Pos2; 3],
    down_polygon: Vec<Pos2>,
    down_arrow: [Pos2; 3],
    left_polygon: Vec<Pos2>,
    left_arrow: [Pos2; 3],
    stroke_color: Color32,
    stroke_width: f32,
    fill_color: Color32,
    arrow_color: Color32,
}

static DPAD_ARTWORK: std::sync::LazyLock<DpadVectorArtwork> =
    std::sync::LazyLock::new(|| DpadVectorArtwork::parse(CONTROLLER_XBOX_SVG));

impl DpadVectorArtwork {
    fn global() -> &'static Self {
        &DPAD_ARTWORK
    }

    fn parse(svg: &str) -> Self {
        let (stroke_color, stroke_width, fill_color, arrow_color) = Self::parse_style(svg);
        let (up_polygon, up_arrow) = Self::parse_direction(svg, "up");
        let (right_polygon, right_arrow) = Self::parse_direction(svg, "right");
        let (down_polygon, down_arrow) = Self::parse_direction(svg, "down");
        let (left_polygon, left_arrow) = Self::parse_direction(svg, "left");

        Self {
            up_polygon,
            up_arrow,
            right_polygon,
            right_arrow,
            down_polygon,
            down_arrow,
            left_polygon,
            left_arrow,
            stroke_color,
            stroke_width,
            fill_color,
            arrow_color,
        }
    }

    #[allow(dead_code)]
    fn palette() -> (Color32, Color32, f32) {
        let art = Self::global();
        (art.stroke_color, art.fill_color, art.stroke_width)
    }

    fn polygon_for(
        &self,
        template: Template,
        rect: Rect,
        shape: HotspotShape,
    ) -> Option<Vec<Pos2>> {
        let (raw_pts, _) = self.raw_direction(shape)?;
        let (offset_x, offset_y) = match template {
            Template::Xbox => (0.0, 0.0),
            Template::Ds4 => (-53.0, -78.0),
        };
        let scale_x = rect.width() / 441.0;
        let scale_y = rect.height() / 403.0;
        Some(
            raw_pts
                .iter()
                .map(|p| {
                    Pos2::new(
                        rect.min.x + (p.x + offset_x) * scale_x,
                        rect.min.y + (p.y + offset_y) * scale_y,
                    )
                })
                .collect(),
        )
    }

    fn arrow_for(&self, template: Template, rect: Rect, shape: HotspotShape) -> Option<[Pos2; 3]> {
        let (_, raw_arrow) = self.raw_direction(shape)?;
        let (offset_x, offset_y) = match template {
            Template::Xbox => (0.0, 0.0),
            Template::Ds4 => (-53.0, -78.0),
        };
        let scale_x = rect.width() / 441.0;
        let scale_y = rect.height() / 403.0;
        let map_pt = |p: &Pos2| {
            Pos2::new(
                rect.min.x + (p.x + offset_x) * scale_x,
                rect.min.y + (p.y + offset_y) * scale_y,
            )
        };
        Some([
            map_pt(&raw_arrow[0]),
            map_pt(&raw_arrow[1]),
            map_pt(&raw_arrow[2]),
        ])
    }

    fn raw_direction(&self, shape: HotspotShape) -> Option<(&[Pos2], &[Pos2; 3])> {
        match shape {
            HotspotShape::DpadUp => Some((&self.up_polygon, &self.up_arrow)),
            HotspotShape::DpadRight => Some((&self.right_polygon, &self.right_arrow)),
            HotspotShape::DpadDown => Some((&self.down_polygon, &self.down_arrow)),
            HotspotShape::DpadLeft => Some((&self.left_polygon, &self.left_arrow)),
            _ => None,
        }
    }

    fn extract_css_block<'a>(svg: &'a str, selector: &str) -> Option<&'a str> {
        let start = svg.find(selector)?;
        let open = svg[start..].find('{')?;
        let close = svg[start + open..].find('}')?;
        Some(&svg[start + open + 1..start + open + close])
    }

    fn parse_style(svg: &str) -> (Color32, f32, Color32, Color32) {
        let mut stroke_color = Color32::from_rgb(193, 215, 238);
        let mut stroke_width = 2.0_f32;
        let mut fill_color = Color32::from_rgba_unmultiplied(193, 215, 238, 46);
        let mut arrow_color = Color32::from_rgb(193, 215, 238);

        if let Some(body) = Self::extract_css_block(svg, ".button") {
            for decl in body.split(';') {
                let parts: Vec<&str> = decl.split(':').map(str::trim).collect();
                if parts.len() == 2 {
                    match parts[0] {
                        "stroke" => {
                            stroke_color = Self::parse_hex_color(parts[1]).unwrap_or(stroke_color);
                        }
                        "stroke-width" => {
                            stroke_width = parts[1].parse::<f32>().unwrap_or(stroke_width);
                        }
                        "fill" => {
                            fill_color = Self::parse_rgba_color(parts[1]).unwrap_or(fill_color);
                        }
                        _ => {}
                    }
                }
            }
        }

        if let Some(body) = Self::extract_css_block(svg, ".arrow") {
            for decl in body.split(';') {
                let parts: Vec<&str> = decl.split(':').map(str::trim).collect();
                if parts.len() == 2 && parts[0] == "fill" {
                    arrow_color = Self::parse_hex_color(parts[1]).unwrap_or(arrow_color);
                }
            }
        }

        (stroke_color, stroke_width, fill_color, arrow_color)
    }

    fn parse_direction(svg: &str, dir_id: &str) -> (Vec<Pos2>, [Pos2; 3]) {
        let id_needle = format!("id=\"dpad-{dir_id}\"");
        let start = svg.find(&id_needle).expect("dpad direction id in SVG");
        let path_slice = &svg[start..];

        let d_start = path_slice.find(" d=\"").expect("path d attribute") + 4;
        let d_end = path_slice[d_start..]
            .find('"')
            .expect("path d closing quote")
            + d_start;
        let d_str = &path_slice[d_start..d_end];
        let polygon = Self::sample_svg_path(d_str, 8);

        let arrow_start = path_slice
            .find("points=\"")
            .expect("arrow points attribute")
            + 8;
        let arrow_end = path_slice[arrow_start..]
            .find('"')
            .expect("arrow points closing quote")
            + arrow_start;
        let points_str = &path_slice[arrow_start..arrow_end];
        let arrow = Self::parse_arrow_points(points_str);

        (polygon, arrow)
    }

    fn sample_svg_path(d: &str, steps: usize) -> Vec<Pos2> {
        let tokens = Self::tokenize_svg_path(d);
        let mut points = Vec::new();
        let mut current = Pos2::ZERO;
        let mut idx = 0;

        while idx < tokens.len() {
            let cmd = &tokens[idx];
            idx += 1;
            match cmd.as_str() {
                "M" | "L" => {
                    if idx + 1 < tokens.len() {
                        let x = tokens[idx].parse::<f32>().unwrap_or(0.0);
                        let y = tokens[idx + 1].parse::<f32>().unwrap_or(0.0);
                        idx += 2;
                        current = Pos2::new(x, y);
                        points.push(current);
                    }
                }
                "C" => {
                    while idx + 5 < tokens.len()
                        && !tokens[idx].starts_with(|c: char| c.is_ascii_alphabetic())
                    {
                        let x1 = tokens[idx].parse::<f32>().unwrap_or(0.0);
                        let y1 = tokens[idx + 1].parse::<f32>().unwrap_or(0.0);
                        let x2 = tokens[idx + 2].parse::<f32>().unwrap_or(0.0);
                        let y2 = tokens[idx + 3].parse::<f32>().unwrap_or(0.0);
                        let x = tokens[idx + 4].parse::<f32>().unwrap_or(0.0);
                        let y = tokens[idx + 5].parse::<f32>().unwrap_or(0.0);
                        idx += 6;

                        let p0 = current;
                        let p1 = Pos2::new(x1, y1);
                        let p2 = Pos2::new(x2, y2);
                        let p3 = Pos2::new(x, y);

                        for step in 1..=steps {
                            let t = step as f32 / steps as f32;
                            let inv_t = 1.0 - t;
                            let bx = inv_t.powi(3) * p0.x
                                + 3.0 * inv_t.powi(2) * t * p1.x
                                + 3.0 * inv_t * t.powi(2) * p2.x
                                + t.powi(3) * p3.x;
                            let by = inv_t.powi(3) * p0.y
                                + 3.0 * inv_t.powi(2) * t * p1.y
                                + 3.0 * inv_t * t.powi(2) * p2.y
                                + t.powi(3) * p3.y;
                            points.push(Pos2::new(bx, by));
                        }
                        current = p3;
                    }
                }
                "Z" | "z" => {}
                _ => {}
            }
        }
        points
    }

    fn parse_arrow_points(points_str: &str) -> [Pos2; 3] {
        let mut coords = Vec::new();
        for item in points_str.split_whitespace() {
            let pair: Vec<&str> = item.split(',').collect();
            if pair.len() == 2 {
                let parsed_x = pair[0].parse::<f32>();
                let parsed_y = pair[1].parse::<f32>();
                if let (Ok(x), Ok(y)) = (parsed_x, parsed_y) {
                    coords.push(Pos2::new(x, y));
                }
            }
        }
        if coords.len() == 3 {
            [coords[0], coords[1], coords[2]]
        } else {
            [Pos2::ZERO, Pos2::ZERO, Pos2::ZERO]
        }
    }

    fn tokenize_svg_path(d: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        for ch in d.chars() {
            if ch.is_ascii_alphabetic() {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                tokens.push(ch.to_string());
            } else if ch.is_ascii_whitespace() || ch == ',' {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            } else if ch == '-' || ch == '+' {
                if !current.is_empty() && !current.ends_with('e') && !current.ends_with('E') {
                    tokens.push(std::mem::take(&mut current));
                }
                current.push(ch);
            } else {
                current.push(ch);
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }
        tokens
    }

    fn parse_hex_color(hex: &str) -> Option<Color32> {
        let hex = hex.trim().strip_prefix('#')?;
        if hex.len() == 6 {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some(Color32::from_rgb(r, g, b))
        } else {
            None
        }
    }

    fn parse_rgba_color(s: &str) -> Option<Color32> {
        let inner = s.trim().strip_prefix("rgba(")?.strip_suffix(')')?;
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() == 4 {
            let r: u8 = parts[0].parse().ok()?;
            let g: u8 = parts[1].parse().ok()?;
            let b: u8 = parts[2].parse().ok()?;
            let a: f32 = parts[3].parse().ok()?;
            Some(Color32::from_rgba_unmultiplied(
                r,
                g,
                b,
                (a * 255.0).round() as u8,
            ))
        } else {
            None
        }
    }
}

impl StudioApp {
    fn new(cc: &eframe::CreationContext<'_>, config: PathBuf, niri_xmb: PathBuf) -> Self {
        configure(&cc.egui_ctx);
        let mut app = Self {
            niri_xmb: niri_xmb.clone(),
            config_path: config.display().to_string(),
            model: Value::Null,
            loaded: Value::Null,
            history_undo: Vec::new(),
            history_redo: Vec::new(),
            history_cursor: Value::Null,
            sha256: String::new(),
            section: Section::Controller,
            show_changes: false,
            show_candidate: false,
            template: Template::Xbox,
            selected: Template::Xbox.default_control(),
            context: "base".to_string(),
            controller_tab: ControllerTab::Bindings,
            popup_tab: PopupTab::PanelPlacements,
            pointer_source: None,
            scroll_source: None,
            error: String::new(),
            candidate: String::new(),
            hot_save: false,
            // Keep the high-volume diagnostic readout out of the way until
            // the user explicitly opens it from the chevron header.
            controller_telemetry_expanded: false,
            controller_pointer_expanded: true,
            controller_trigger_expanded: true,
            display_pointer_multiplier: 1.0,
            display_scroll_multiplier: 1.0,
            display_multiplier_at: Instant::now(),
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
            preview_pinned: false,
            keymap: keymap::NiriKeymap::load_default(),
            keymap_at: Instant::now(),
            keymap_search: String::new(),
            keymap_selected: None,
            field_errors: Vec::new(),
            validated_model: Value::Null,
            validated_path: String::new(),
        };
        app.live_thread = Some(spawn_live(
            cc.egui_ctx.clone(),
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
            self.show_changes = false;
            self.show_candidate = false;
            return;
        }
        self.model = result["config"].clone();
        self.loaded = self.model.clone();
        self.history_undo.clear();
        self.history_redo.clear();
        self.history_cursor = self.model.clone();
        self.sha256 = result["sha256"].as_str().unwrap_or("").to_string();
        self.config_path = result["path"]
            .as_str()
            .unwrap_or(&self.config_path)
            .to_string();
        self.error.clear();
        self.candidate.clear();
        self.show_changes = false;
        self.show_candidate = false;
    }

    fn dirty(&self) -> bool {
        self.model != Value::Null && self.model != self.loaded
    }

    /// Recompute inline field errors from the current model. Runs once per
    /// frame; the daemon validator is the single source of truth.
    fn refresh_validation(&mut self) {
        if self.validated_model == self.model && self.validated_path == self.config_path {
            return;
        }
        self.validated_model = self.model.clone();
        self.validated_path = self.config_path.clone();
        self.field_errors = validation::field_errors(&self.model);
        if let Some(theme) = get_path(&self.model, "theme").and_then(Value::as_str)
            && let Some(message) = validation::theme_error(theme, &self.config_path)
            && !self.field_errors.iter().any(|error| error.path == "theme")
        {
            self.field_errors.push(validation::FieldError {
                path: "theme".to_string(),
                message,
            });
        }
    }

    fn field_error(&self, path: &str) -> Option<&str> {
        self.field_errors
            .iter()
            .find(|error| error.path == path || error.path.starts_with(&format!("{path}.")))
            .map(|error| error.message.as_str())
    }

    fn show_field_error(&self, ui: &mut Ui, path: &str) {
        if let Some(message) = self.field_error(path) {
            ui.label(RichText::new(message).size(10.0).color(ERROR));
        }
    }

    fn show_exact_field_error(&self, ui: &mut Ui, path: &str) {
        if let Some(error) = self.field_errors.iter().find(|error| error.path == path) {
            ui.label(RichText::new(&error.message).size(10.0).color(ERROR));
        }
    }

    fn show_errors_for_prefix(&self, ui: &mut Ui, path: &str) {
        for error in &self.field_errors {
            if error.path == path || error.path.starts_with(&format!("{path}.")) {
                ui.label(RichText::new(&error.message).size(10.0).color(ERROR));
            }
        }
    }

    /// Gate Validate/Save on inline errors so the daemon validator is a
    /// backstop, not the first place a bad field is reported.
    fn inline_error(&mut self) -> Option<String> {
        self.refresh_validation();
        if self.field_errors.is_empty() {
            return None;
        }
        let first = self.field_errors[0].message.clone();
        Some(if self.field_errors.len() > 1 {
            format!(
                "{first} (+{} more invalid fields)",
                self.field_errors.len() - 1
            )
        } else {
            first
        })
    }

    fn output_seed(&self, list_path: &str) -> String {
        let used = get_path(&self.model, list_path)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.get("output").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let live_outputs = self
            .live
            .lock()
            .ok()
            .and_then(|live| snapshot_from_json(&live.snapshot))
            .map(|snapshot| snapshot.outputs.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        validation::output_seed(&used, &live_outputs)
    }

    /// Capture one completed UI edit. Field editors mutate `model` directly;
    /// comparing once per frame keeps history centralized and groups a drag
    /// or text edit into a single useful step.
    fn sync_history(&mut self) {
        if self.model != self.history_cursor {
            self.history_undo.push(self.history_cursor.clone());
            if self.history_undo.len() > 50 {
                self.history_undo.remove(0);
            }
            self.history_cursor = self.model.clone();
            self.history_redo.clear();
            if self.hot_save {
                self.save(true);
            }
        }
    }

    fn undo(&mut self) {
        if let Some(previous) = self.history_undo.pop() {
            self.history_redo.push(self.model.clone());
            self.model = previous;
            self.history_cursor = self.model.clone();
            self.candidate.clear();
        }
    }

    fn redo(&mut self) {
        if let Some(next) = self.history_redo.pop() {
            self.history_undo.push(self.model.clone());
            self.model = next;
            self.history_cursor = self.model.clone();
            self.candidate.clear();
        }
    }

    fn patch(&self) -> Patch {
        compute_patch(&self.loaded, &self.model)
    }

    fn validate(&mut self) {
        if self.model == Value::Null {
            return;
        }
        if let Some(error) = self.inline_error() {
            self.error = error;
            return;
        }
        if let Some(error) = self.trigger_points_error() {
            self.error = error;
            return;
        }
        let patch = self.patch();
        if patch.is_empty() {
            self.error.clear();
            self.candidate.clear();
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
        } else {
            self.error = result["error"]
                .as_str()
                .unwrap_or("validation failed")
                .to_string();
            self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
        }
    }

    fn save(&mut self, restart_after: bool) {
        if self.model == Value::Null {
            return;
        }
        if let Some(error) = self.inline_error() {
            self.error = error;
            return;
        }
        if let Some(error) = self.trigger_points_error() {
            self.error = error;
            return;
        }
        let patch = self.patch();
        if patch.is_empty() {
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
            return;
        }
        self.loaded = self.model.clone();
        self.sha256 = result["sha256"].as_str().unwrap_or("").to_string();
        self.candidate = result["candidate"].as_str().unwrap_or("").to_string();
        self.error.clear();
        self.show_changes = false;
        self.show_candidate = false;
        if restart_after {
            let result = restart(&self.niri_xmb, &json!({"path": self.config_path}));
            if result["ok"] != true {
                self.error = result["error"]
                    .as_str()
                    .unwrap_or("restart failed")
                    .to_string();
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
        let (dot, text) = if live.status.is_null() && !live.stream_alive {
            (TEXT_DIM, "daemon offline".to_string())
        } else if !live.stream_alive {
            (WARN, format!("reconnecting · attempt {}", live.reconnects))
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
        let dot = if !self.error.is_empty() || !self.field_errors.is_empty() {
            ERROR
        } else if !self.patch().is_empty() {
            WARN
        } else {
            OK
        };
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, dot);
    }

    fn action_buttons(&mut self, ui: &mut Ui) {
        let dirty = self.dirty() && self.field_errors.is_empty();
        let can_undo = !self.history_undo.is_empty();
        let can_redo = !self.history_redo.is_empty();
        if history_icon_button(ui, can_undo, false, "ui.action.undo").clicked() {
            self.undo();
        }
        if history_icon_button(ui, can_redo, true, "ui.action.redo").clicked() {
            self.redo();
        }
        ui.separator();
        if tt(
            ui.selectable_label(self.show_changes, "☷ Changes"),
            "ui.action.changes",
        )
        .clicked()
        {
            self.show_changes = !self.show_changes;
        }
        if tt(
            ui.add_enabled(self.model != Value::Null, egui::Button::new("✓ Validate")),
            "ui.action.validate",
        )
        .clicked()
        {
            self.validate();
        }
        let hot_save_changed = ui
            .checkbox(&mut self.hot_save, "Hot save")
            .on_hover_text("Automatically save and restart after each valid edit")
            .changed();
        if hot_save_changed && self.hot_save && dirty {
            self.save(true);
        }
        if tt(
            ui.add_enabled(dirty, egui::Button::new("Save")),
            "ui.action.save_restart",
        )
        .clicked()
        {
            self.save(true);
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
        if !patch.is_empty() {
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
                            Section::Popup => self.popup_section(ui),
                            Section::Keyboard => self.keyboard_section(ui),
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

        let live = self
            .live
            .lock()
            .map(|live| live.clone())
            .unwrap_or_default();
        let view = GamepadLiveView::from_status(&live.status);

        let width = ui.available_width();
        if width >= 960.0 {
            let stage_width = (width * 0.44).clamp(420.0, 560.0);
            let mut hotspot = None;
            let mut row = None;
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(stage_width);
                    card(ui, |ui| {
                        hotspot = self.controller_stage(ui, &live, &view);
                    });
                });
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    card(ui, |ui| {
                        row = self.controller_tabs_panel(ui, &live, &view);
                    });
                });
            });
            if let (Some(from), Some(to)) = (hotspot, row)
                && self.controller_tab == ControllerTab::Bindings
            {
                paint_binding_link(ui, from, to);
            }
        } else {
            let mut hotspot = None;
            let mut row = None;
            card(ui, |ui| {
                hotspot = self.controller_stage(ui, &live, &view);
            });
            ui.add_space(4.0);
            card(ui, |ui| {
                row = self.controller_tabs_panel(ui, &live, &view);
            });
            if let (Some(from), Some(to)) = (hotspot, row)
                && self.controller_tab == ControllerTab::Bindings
            {
                paint_binding_link(ui, from, to);
            }
        }
    }

    /// Persistent controller visualizer stage: device identity, template switch,
    /// interactive SVG artwork with live stick deflections and trigger meters,
    /// paddle strip, and immediate inspector/quick-mapper for selected controls.
    fn controller_stage(
        &mut self,
        ui: &mut Ui,
        live: &LiveState,
        view: &GamepadLiveView,
    ) -> Option<Rect> {
        self.controller_stage_header(ui, live, view);
        ui.add_space(8.0);
        let hotspot = self.controller_map_body(ui, view);
        ui.add_space(6.0);
        self.controller_paddles_strip(ui, view);
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        self.inspector_body(ui);
        hotspot
    }

    /// Header on the persistent controller stage: connection indicator, device name,
    /// vendor/product IDs, template toggle (Xbox/DS4), and live stream telemetry rate.
    fn controller_stage_header(&mut self, ui: &mut Ui, live: &LiveState, view: &GamepadLiveView) {
        let is_connected = view.connected > 0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if view.device_name.is_empty() {
                    "No controller"
                } else {
                    &view.device_name
                })
                .size(13.0)
                .strong(),
            );
            let dot = if is_connected { OK } else { WARN };
            let (dot_rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
            ui.painter().circle_filled(dot_rect.center(), 3.0, dot);
            ui.label(
                RichText::new(if is_connected {
                    "connected"
                } else {
                    "not detected"
                })
                .size(10.0)
                .color(dot),
            );
            if is_connected && (view.vendor != 0 || view.product != 0) {
                ui.add_space(2.0);
                ui.label(
                    RichText::new(format!("[{:04x}:{:04x}]", view.vendor, view.product))
                        .monospace()
                        .size(9.5)
                        .color(TEXT_DIM),
                );
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                for template in [Template::Ds4, Template::Xbox] {
                    if ui
                        .selectable_label(self.template == template, template.label())
                        .clicked()
                    {
                        self.switch_template(template);
                    }
                }
            });
        });
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            let rate = if live.gamepad_hz > 0.5 {
                format!("{:.0} Hz", live.gamepad_hz)
            } else {
                "idle".to_string()
            };
            let count = self.connected_count();
            let count_str = if count > 1 {
                format!("{count} devices · ")
            } else {
                String::new()
            };
            ui.label(
                RichText::new(format!(
                    "{count_str}{} · {}",
                    rate,
                    self.gamepad_mode_name()
                ))
                .monospace()
                .size(9.5)
                .color(TEXT_DIM),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(error) = live.status["error"].as_str() {
                    ui.label(RichText::new(error).size(9.5).color(ERROR));
                } else if !live.error.is_empty() {
                    ui.label(RichText::new(&live.error).size(9.5).color(ERROR));
                } else {
                    ui.label(
                        RichText::new("click button to inspect & map")
                            .size(9.0)
                            .color(TEXT_DIM),
                    );
                }
            });
        });
    }

    /// Segmented tabs panel: tab bar organizing Bindings & Layers, Analog & Curves,
    /// Live Diagnostics, and Device Hardware settings.
    fn controller_tabs_panel(
        &mut self,
        ui: &mut Ui,
        live: &LiveState,
        view: &GamepadLiveView,
    ) -> Option<Rect> {
        let mut row = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for tab in [
                ControllerTab::Bindings,
                ControllerTab::Curves,
                ControllerTab::Diagnostics,
                ControllerTab::Device,
            ] {
                self.controller_tab_button(ui, tab);
            }
        });
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        match self.controller_tab {
            ControllerTab::Bindings => {
                let nav_rect = self.pointer_scroll_navigation_pods(ui);
                ui.add_space(8.0);
                ui.separator();
                ui.add_space(6.0);
                let bindings_rect = self.merged_bindings(ui);
                row = nav_rect.or(bindings_rect);
            }
            ControllerTab::Curves => {
                self.controller_curves_body(ui);
            }
            ControllerTab::Diagnostics => {
                self.gamepad_diagnostics_body(ui, live, view);
            }
            ControllerTab::Device => {
                self.device_fields(ui);
            }
        }
        row
    }

    fn is_left_stick_selected(&self) -> bool {
        is_left_stick(self.selected)
    }

    fn is_right_stick_selected(&self) -> bool {
        is_right_stick(self.selected)
    }

    fn select_stick(&mut self, stick: &str) {
        self.selected = stick_control_for_template(self.template, stick);
    }

    fn buttons_for_action(&self, context: &str, action: &str) -> Vec<String> {
        let path = if context == "xmb" {
            "gamepad.navi.bindings".to_string()
        } else {
            binding_path(context)
        };
        get_path(&self.model, &path)
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter(|(_, act)| act.as_str() == Some(action))
                    .map(|(code, _)| code.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn control_name_for_code(&self, code: &str) -> &'static str {
        control_name_for_code_and_template(self.template, code)
    }

    /// Two side-by-side cards at the top of the Bindings tab:
    /// Left: Pointer (Cursor), Right: Scroll Wheel.
    /// Returns the target Rect when the selected controller element (stick or button)
    /// maps to pointer or scroll, enabling bezier line connection.
    fn pointer_scroll_navigation_pods(&mut self, ui: &mut Ui) -> Option<Rect> {
        let width = ui.available_width();
        let mut target_rect = None;
        if width >= 480.0 {
            ui.columns(2, |cols| {
                let p_rect = self.pointer_pod_ui(&mut cols[0]);
                let s_rect = self.scroll_pod_ui(&mut cols[1]);
                target_rect = p_rect.or(s_rect);
            });
        } else {
            let p_rect = self.pointer_pod_ui(ui);
            ui.add_space(6.0);
            let s_rect = self.scroll_pod_ui(ui);
            target_rect = p_rect.or(s_rect);
        }
        target_rect
    }

    fn pointer_control_source(&self) -> ControlSource {
        if let Some(src) = self.pointer_source {
            return src;
        }
        let map_path = binding_path(&self.context);
        let bindings = get_path(&self.model, &map_path).and_then(Value::as_object);
        let has_dpad_move = bindings.is_some_and(|map| {
            map.get("BTN_DPAD_UP").and_then(Value::as_str) == Some("pointer.move-up")
        });
        if has_dpad_move {
            return ControlSource::Dpad;
        }
        let has_other_move = bindings.is_some_and(|map| {
            map.values().any(|v| {
                v.as_str().is_some_and(|s| {
                    matches!(
                        s,
                        "pointer.move-up"
                            | "pointer.move-down"
                            | "pointer.move-left"
                            | "pointer.move-right"
                    )
                })
            })
        });
        if has_other_move {
            return ControlSource::Other;
        }
        let stick = get_path(&self.model, "gamepad.navi.pointer.stick")
            .and_then(Value::as_str)
            .unwrap_or("left");
        if stick == "right" {
            ControlSource::RightStick
        } else {
            ControlSource::LeftStick
        }
    }

    fn set_pointer_control_source(&mut self, src: ControlSource) {
        self.pointer_source = Some(src);
        match src {
            ControlSource::LeftStick => {
                set_json_path(
                    &mut self.model,
                    "gamepad.navi.pointer.stick",
                    Value::String("left".to_string()),
                );
                self.select_stick("left");
            }
            ControlSource::RightStick => {
                set_json_path(
                    &mut self.model,
                    "gamepad.navi.pointer.stick",
                    Value::String("right".to_string()),
                );
                self.select_stick("right");
            }
            ControlSource::Dpad => {
                let map_path = if self.context == "xmb" {
                    "gamepad.navi.bindings".to_string()
                } else {
                    binding_path(&self.context)
                };
                set_map_value(&mut self.model, &map_path, "BTN_DPAD_UP", "pointer.move-up");
                set_map_value(
                    &mut self.model,
                    &map_path,
                    "BTN_DPAD_DOWN",
                    "pointer.move-down",
                );
                set_map_value(
                    &mut self.model,
                    &map_path,
                    "BTN_DPAD_LEFT",
                    "pointer.move-left",
                );
                set_map_value(
                    &mut self.model,
                    &map_path,
                    "BTN_DPAD_RIGHT",
                    "pointer.move-right",
                );
                self.selected = "D-pad up";
            }
            ControlSource::Other => {}
        }
    }

    fn scroll_control_source(&self) -> ControlSource {
        if let Some(src) = self.scroll_source {
            return src;
        }
        let map_path = binding_path(&self.context);
        let bindings = get_path(&self.model, &map_path).and_then(Value::as_object);
        let has_dpad_scroll = bindings.is_some_and(|map| {
            map.get("BTN_DPAD_UP").and_then(Value::as_str) == Some("pointer.scroll-up")
        });
        if has_dpad_scroll {
            return ControlSource::Dpad;
        }
        let has_other_scroll = bindings.is_some_and(|map| {
            map.values().any(|v| {
                v.as_str()
                    .is_some_and(|s| matches!(s, "pointer.scroll-up" | "pointer.scroll-down"))
            })
        });
        if has_other_scroll {
            return ControlSource::Other;
        }
        let stick = get_path(&self.model, "gamepad.navi.pointer.scroll_stick")
            .and_then(Value::as_str)
            .unwrap_or("right");
        if stick == "left" {
            ControlSource::LeftStick
        } else {
            ControlSource::RightStick
        }
    }

    fn set_scroll_control_source(&mut self, src: ControlSource) {
        self.scroll_source = Some(src);
        match src {
            ControlSource::LeftStick => {
                set_json_path(
                    &mut self.model,
                    "gamepad.navi.pointer.scroll_stick",
                    Value::String("left".to_string()),
                );
                self.select_stick("left");
            }
            ControlSource::RightStick => {
                set_json_path(
                    &mut self.model,
                    "gamepad.navi.pointer.scroll_stick",
                    Value::String("right".to_string()),
                );
                self.select_stick("right");
            }
            ControlSource::Dpad => {
                let map_path = if self.context == "xmb" {
                    "gamepad.navi.bindings".to_string()
                } else {
                    binding_path(&self.context)
                };
                set_map_value(
                    &mut self.model,
                    &map_path,
                    "BTN_DPAD_UP",
                    "pointer.scroll-up",
                );
                set_map_value(
                    &mut self.model,
                    &map_path,
                    "BTN_DPAD_DOWN",
                    "pointer.scroll-down",
                );
                self.selected = "D-pad up";
            }
            ControlSource::Other => {}
        }
    }

    fn pointer_pod_ui(&mut self, ui: &mut Ui) -> Option<Rect> {
        let current_source = self.pointer_control_source();
        let stick_selected = match current_source {
            ControlSource::LeftStick => self.is_left_stick_selected(),
            ControlSource::RightStick => self.is_right_stick_selected(),
            _ => false,
        };

        let selected_code = self.selected_code().map(str::to_string);
        let mut target_rect = None;

        let selected_action = selected_code.as_deref().and_then(|code| {
            let map_path = binding_path(&self.context);
            get_path(&self.model, &map_path)
                .and_then(Value::as_object)
                .and_then(|map| map.get(code))
                .and_then(Value::as_str)
        });
        let click_matches = matches!(
            selected_action,
            Some(
                "pointer.left"
                    | "pointer.right"
                    | "pointer.middle"
                    | "pointer.back"
                    | "pointer.forward"
            )
        );
        let motion_matches = (current_source == ControlSource::Dpad
            || current_source == ControlSource::Other)
            && matches!(
                selected_action,
                Some(
                    "pointer.move-up"
                        | "pointer.move-down"
                        | "pointer.move-left"
                        | "pointer.move-right"
                )
            );
        let is_active = stick_selected || click_matches || motion_matches;

        let frame = egui::Frame::new()
            .fill(FIELD_BG)
            .stroke(Stroke::new(
                if is_active { 1.5_f32 } else { 1.0_f32 },
                if is_active { ACCENT } else { BORDER },
            ))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::same(8));

        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Header: Cursor icon, title, Enabled checkbox, Curves jump
            ui.horizontal(|ui| {
                let (icon_rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::hover());
                draw_cursor_icon(
                    ui.painter(),
                    icon_rect,
                    if is_active { ACCENT } else { TEXT },
                );
                ui.add_space(2.0);
                ui.label(RichText::new("Pointer").size(12.5).strong());

                let mut enabled = get_path(&self.model, "gamepad.navi.pointer.enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if ui
                    .checkbox(&mut enabled, "")
                    .on_hover_text("Enable analog stick or digital mouse cursor")
                    .changed()
                {
                    set_json_path(
                        &mut self.model,
                        "gamepad.navi.pointer.enabled",
                        Value::Bool(enabled),
                    );
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button("∿ Curves")
                        .on_hover_text("Open analog response curves & deadzones")
                        .clicked()
                    {
                        self.controller_tab = ControllerTab::Curves;
                    }
                });
            });

            ui.add_space(4.0);

            // Source dropdown & Invert Y
            ui.horizontal(|ui| {
                ui.label(RichText::new("Source:").size(10.5).color(TEXT_MUTED));
                let mut chosen_source = current_source;
                let combo = ComboBox::from_id_salt("pointer-source-dropdown")
                    .width(105.0)
                    .selected_text(current_source.label())
                    .show_ui(ui, |ui| {
                        for src in [
                            ControlSource::LeftStick,
                            ControlSource::RightStick,
                            ControlSource::Dpad,
                            ControlSource::Other,
                        ] {
                            if ui
                                .selectable_value(&mut chosen_source, src, src.label())
                                .clicked()
                            {
                                self.set_pointer_control_source(src);
                            }
                        }
                    });
                if stick_selected {
                    target_rect = Some(combo.response.rect);
                }

                ui.add_space(6.0);
                let mut invert_y = get_path(&self.model, "gamepad.navi.pointer.invert_y")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if ui
                    .checkbox(&mut invert_y, "Invert Y")
                    .on_hover_text("Invert pointer vertical axis")
                    .changed()
                {
                    set_json_path(
                        &mut self.model,
                        "gamepad.navi.pointer.invert_y",
                        Value::Bool(invert_y),
                    );
                }
            });

            // Motion row if D-pad or Other
            if current_source == ControlSource::Dpad || current_source == ControlSource::Other {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Motion:").size(10.5).color(TEXT_MUTED));
                    for (act, act_label) in [
                        ("pointer.move-up", "Up"),
                        ("pointer.move-down", "Down"),
                        ("pointer.move-left", "Left"),
                        ("pointer.move-right", "Right"),
                    ] {
                        let bound = self.buttons_for_action(&self.context, act);
                        if bound.is_empty() {
                            let text = RichText::new(format!("+ {act_label}"))
                                .size(9.5)
                                .color(TEXT_DIM);
                            let btn = egui::Button::new(text)
                                .fill(CARD)
                                .stroke(Stroke::new(1.0_f32, BORDER))
                                .corner_radius(3.0);
                            let mut pill_resp = ui.add(btn);
                            if let Some(code) = &selected_code {
                                pill_resp =
                                    pill_resp.on_hover_text(format!("Bind {code} to {act_label}"));
                            }
                            if pill_resp.clicked()
                                && let Some(code) = self.selected_code()
                            {
                                let map_path = if self.context == "xmb" {
                                    "gamepad.navi.bindings".to_string()
                                } else {
                                    binding_path(&self.context)
                                };
                                set_map_value(&mut self.model, &map_path, code, act);
                            }
                        } else {
                            for code in &bound {
                                let ctrl = self.control_name_for_code(code);
                                let is_sel = self.selected == ctrl
                                    || selected_code.as_deref() == Some(code.as_str());
                                let text = RichText::new(format!("{act_label}: {ctrl}"))
                                    .size(9.5)
                                    .color(if is_sel { Color32::WHITE } else { TEXT_MUTED });
                                let btn = egui::Button::new(text)
                                    .fill(if is_sel {
                                        Color32::from_rgba_unmultiplied(116, 162, 255, 50)
                                    } else {
                                        CARD
                                    })
                                    .stroke(if is_sel {
                                        Stroke::new(1.5_f32, ACCENT)
                                    } else {
                                        Stroke::new(1.0_f32, BORDER)
                                    })
                                    .corner_radius(3.0);
                                let pill_resp = ui.add(btn);
                                if is_sel {
                                    target_rect = Some(pill_resp.rect);
                                }
                                if pill_resp.clicked() {
                                    self.selected = ctrl;
                                }
                            }
                        }
                    }
                });
            }

            ui.add_space(4.0);

            // Clicks row (Occupying buttons)
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Clicks:").size(10.5).color(TEXT_MUTED));
                for (act, act_label) in [
                    ("pointer.left", "L-Click"),
                    ("pointer.right", "R-Click"),
                    ("pointer.middle", "M-Click"),
                    ("pointer.back", "M4"),
                    ("pointer.forward", "M5"),
                ] {
                    let bound = self.buttons_for_action(&self.context, act);
                    if bound.is_empty() {
                        let text = RichText::new(format!("+ {act_label}"))
                            .size(9.5)
                            .color(TEXT_DIM);
                        let btn = egui::Button::new(text)
                            .fill(CARD)
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .corner_radius(3.0);
                        let mut pill_resp = ui.add(btn);
                        if let Some(code) = &selected_code {
                            pill_resp =
                                pill_resp.on_hover_text(format!("Bind {code} to {act_label}"));
                        }
                        if pill_resp.clicked()
                            && let Some(code) = self.selected_code()
                        {
                            let map_path = if self.context == "xmb" {
                                "gamepad.navi.bindings".to_string()
                            } else {
                                binding_path(&self.context)
                            };
                            set_map_value(&mut self.model, &map_path, code, act);
                        }
                    } else {
                        for code in &bound {
                            let ctrl = self.control_name_for_code(code);
                            let is_sel = self.selected == ctrl
                                || selected_code.as_deref() == Some(code.as_str());
                            let text = RichText::new(format!("{act_label}: {ctrl}"))
                                .size(9.5)
                                .color(if is_sel { Color32::WHITE } else { TEXT_MUTED });
                            let btn = egui::Button::new(text)
                                .fill(if is_sel {
                                    Color32::from_rgba_unmultiplied(116, 162, 255, 50)
                                } else {
                                    CARD
                                })
                                .stroke(if is_sel {
                                    Stroke::new(1.5_f32, ACCENT)
                                } else {
                                    Stroke::new(1.0_f32, BORDER)
                                })
                                .corner_radius(3.0);
                            let pill_resp = ui.add(btn);
                            if is_sel {
                                target_rect = Some(pill_resp.rect);
                            }
                            if pill_resp.clicked() {
                                self.selected = ctrl;
                            }
                        }
                    }
                }
            });
        });

        target_rect
    }

    fn scroll_pod_ui(&mut self, ui: &mut Ui) -> Option<Rect> {
        let current_source = self.scroll_control_source();
        let stick_selected = match current_source {
            ControlSource::LeftStick => self.is_left_stick_selected(),
            ControlSource::RightStick => self.is_right_stick_selected(),
            _ => false,
        };

        let selected_code = self.selected_code().map(str::to_string);
        let mut target_rect = None;

        let selected_action = selected_code.as_deref().and_then(|code| {
            let map_path = binding_path(&self.context);
            get_path(&self.model, &map_path)
                .and_then(Value::as_object)
                .and_then(|map| map.get(code))
                .and_then(Value::as_str)
        });
        let button_matches = matches!(
            selected_action,
            Some("pointer.scroll-up" | "pointer.scroll-down")
        );
        let is_active = stick_selected || button_matches;

        let frame = egui::Frame::new()
            .fill(FIELD_BG)
            .stroke(Stroke::new(
                if is_active { 1.5_f32 } else { 1.0_f32 },
                if is_active { ACCENT } else { BORDER },
            ))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::same(8));

        frame.show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Header: Scroll wheel icon, title, Enabled checkbox, Curves jump
            ui.horizontal(|ui| {
                let (icon_rect, _) = ui.allocate_exact_size(Vec2::new(18.0, 18.0), Sense::hover());
                draw_scroll_wheel_icon(
                    ui.painter(),
                    icon_rect,
                    if is_active { ACCENT } else { TEXT },
                );
                ui.add_space(2.0);
                ui.label(RichText::new("Scroll Wheel").size(12.5).strong());

                let mut enabled = get_path(&self.model, "gamepad.navi.pointer.scroll_enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if ui
                    .checkbox(&mut enabled, "")
                    .on_hover_text("Enable scroll wheel")
                    .changed()
                {
                    set_json_path(
                        &mut self.model,
                        "gamepad.navi.pointer.scroll_enabled",
                        Value::Bool(enabled),
                    );
                }

                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .small_button("∿ Curves")
                        .on_hover_text("Open analog response curves & deadzones")
                        .clicked()
                    {
                        self.controller_tab = ControllerTab::Curves;
                    }
                });
            });

            ui.add_space(4.0);

            // Source dropdown & Invert Y
            ui.horizontal(|ui| {
                ui.label(RichText::new("Source:").size(10.5).color(TEXT_MUTED));
                let mut chosen_source = current_source;
                let combo = ComboBox::from_id_salt("scroll-source-dropdown")
                    .width(105.0)
                    .selected_text(current_source.label())
                    .show_ui(ui, |ui| {
                        for src in [
                            ControlSource::RightStick,
                            ControlSource::LeftStick,
                            ControlSource::Dpad,
                            ControlSource::Other,
                        ] {
                            if ui
                                .selectable_value(&mut chosen_source, src, src.label())
                                .clicked()
                            {
                                self.set_scroll_control_source(src);
                            }
                        }
                    });
                if stick_selected {
                    target_rect = Some(combo.response.rect);
                }

                ui.add_space(6.0);
                let mut scroll_invert_y =
                    get_path(&self.model, "gamepad.navi.pointer.scroll_invert_y")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                if ui
                    .checkbox(&mut scroll_invert_y, "Invert Y")
                    .on_hover_text("Invert scroll wheel direction")
                    .changed()
                {
                    set_json_path(
                        &mut self.model,
                        "gamepad.navi.pointer.scroll_invert_y",
                        Value::Bool(scroll_invert_y),
                    );
                }
            });

            ui.add_space(4.0);

            // Ticks row (Occupying buttons)
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Ticks:").size(10.5).color(TEXT_MUTED));
                for (act, act_label) in [
                    ("pointer.scroll-up", "Scroll ▲"),
                    ("pointer.scroll-down", "Scroll ▼"),
                ] {
                    let bound = self.buttons_for_action(&self.context, act);
                    if bound.is_empty() {
                        let text = RichText::new(format!("+ {act_label}"))
                            .size(9.5)
                            .color(TEXT_DIM);
                        let btn = egui::Button::new(text)
                            .fill(CARD)
                            .stroke(Stroke::new(1.0_f32, BORDER))
                            .corner_radius(3.0);
                        let mut pill_resp = ui.add(btn);
                        if let Some(code) = &selected_code {
                            pill_resp =
                                pill_resp.on_hover_text(format!("Bind {code} to {act_label}"));
                        }
                        if pill_resp.clicked()
                            && let Some(code) = self.selected_code()
                        {
                            let map_path = if self.context == "xmb" {
                                "gamepad.navi.bindings".to_string()
                            } else {
                                binding_path(&self.context)
                            };
                            set_map_value(&mut self.model, &map_path, code, act);
                        }
                    } else {
                        for code in &bound {
                            let ctrl = self.control_name_for_code(code);
                            let is_sel = self.selected == ctrl
                                || selected_code.as_deref() == Some(code.as_str());
                            let text = RichText::new(format!("{act_label}: {ctrl}"))
                                .size(9.5)
                                .color(if is_sel { Color32::WHITE } else { TEXT_MUTED });
                            let btn = egui::Button::new(text)
                                .fill(if is_sel {
                                    Color32::from_rgba_unmultiplied(116, 162, 255, 50)
                                } else {
                                    CARD
                                })
                                .stroke(if is_sel {
                                    Stroke::new(1.5_f32, ACCENT)
                                } else {
                                    Stroke::new(1.0_f32, BORDER)
                                })
                                .corner_radius(3.0);
                            let pill_resp = ui.add(btn);
                            if is_sel {
                                target_rect = Some(pill_resp.rect);
                            }
                            if pill_resp.clicked() {
                                self.selected = ctrl;
                            }
                        }
                    }
                }
            });
        });

        target_rect
    }

    fn controller_tab_button(&mut self, ui: &mut Ui, tab: ControllerTab) {
        let is_selected = self.controller_tab == tab;
        let text = RichText::new(format!("{} {}", tab.icon(), tab.label()))
            .size(11.5)
            .color(if is_selected {
                Color32::WHITE
            } else {
                TEXT_MUTED
            });
        let stroke = if is_selected {
            Stroke::new(1.2_f32, ACCENT)
        } else {
            Stroke::new(1.0_f32, BORDER)
        };
        let fill = if is_selected {
            Color32::from_rgba_unmultiplied(116, 162, 255, 30)
        } else {
            FIELD_BG
        };
        let button = egui::Button::new(text)
            .fill(fill)
            .stroke(stroke)
            .corner_radius(4.0)
            .min_size(Vec2::new(0.0, 24.0));
        if ui.add(button).clicked() {
            self.controller_tab = tab;
        }
    }

    fn controller_curves_body(&mut self, ui: &mut Ui) {
        ui.label(
            RichText::new("Analog sticks motion, scrolling, and trigger response curves.")
                .size(11.0)
                .color(TEXT_DIM),
        );
        ui.add_space(6.0);
        if collapsible_heading(
            ui,
            "Pointer from analog sticks",
            self.controller_pointer_expanded,
        ) {
            self.controller_pointer_expanded = !self.controller_pointer_expanded;
        }
        if self.controller_pointer_expanded {
            self.pointer_fields(ui);
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(4.0);
        }
        if collapsible_heading(
            ui,
            "Trigger speed: brake / gas",
            self.controller_trigger_expanded,
        ) {
            self.controller_trigger_expanded = !self.controller_trigger_expanded;
        }
        if self.controller_trigger_expanded {
            self.trigger_response(ui);
            self.accel_fields(ui);
        }
    }

    /// Tester-style live readout: device identity, every button value,
    /// both stick reticles with raw axis numbers, and the stream rate.
    /// Fed by pushed gamepad frames; renders identically at any width by
    /// wrapping button cells and stacking the stick row when narrow.
    fn gamepad_diagnostics_body(&self, ui: &mut Ui, live: &LiveState, view: &GamepadLiveView) {
        diagnostics_label(ui, "Raw I/O & Axis Trace");
        let is_connected = view.connected > 0;
        if is_connected && (view.vendor != 0 || view.product != 0) {
            ui.label(
                RichText::new(format!(
                    "Vendor: {:04x}  Product: {:04x}  ·  {}",
                    view.vendor, view.product, view.device_path
                ))
                .monospace()
                .size(9.5)
                .color(TEXT_DIM),
            );
        } else if is_connected && !view.device_path.is_empty() {
            ui.label(
                RichText::new(&view.device_path)
                    .monospace()
                    .size(9.5)
                    .color(TEXT_DIM),
            );
        }
        ui.add_space(6.0);
        let rate = if live.gamepad_hz > 0.5 {
            format!("{:.0} Hz", live.gamepad_hz)
        } else {
            "idle".to_string()
        };
        ui.horizontal_wrapped(|ui| {
            metric_chip(ui, "MAPPING", &view.preset);
            metric_chip(ui, "MODE", self.gamepad_mode_name());
            metric_chip(ui, "DEVICES", view.connected);
            metric_chip(ui, "STREAM", rate);
            metric_chip(ui, "FRAMES", live.gamepad_frames);
        });
        if !is_connected {
            ui.add_space(6.0);
            ui.label(
                RichText::new("Connect a controller to see live buttons, sticks and triggers.")
                    .size(11.0)
                    .color(TEXT_MUTED),
            );
            return;
        }
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        diagnostics_label(ui, "Buttons");
        ui.horizontal_wrapped(|ui| {
            for (code, friendly) in BUTTONS {
                // D-pad may arrive as hat axes rather than BTN_DPAD_*.
                let pressed = match *code {
                    "BTN_DPAD_UP" => view.dpad("up"),
                    "BTN_DPAD_DOWN" => view.dpad("down"),
                    "BTN_DPAD_LEFT" => view.dpad("left"),
                    "BTN_DPAD_RIGHT" => view.dpad("right"),
                    _ => view.button(code),
                };
                let short = friendly.split(" / ").next().unwrap_or(friendly);
                button_cell(ui, short, pressed);
            }
        });
        ui.add_space(8.0);
        let (lx, ly) = view.stick(true);
        let (rx, ry) = view.stick(false);
        if ui.available_width() >= 680.0 {
            ui.horizontal_top(|ui| {
                stick_reticle(ui, "L STICK", "AXIS 0", lx, "AXIS 1", ly);
                ui.add_space(18.0);
                stick_reticle(ui, "R STICK", "AXIS 2", rx, "AXIS 3", ry);
                ui.add_space(18.0);
                ui.vertical(|ui| {
                    diagnostics_label(ui, "Axes & triggers");
                    axes_list(ui, view);
                });
            });
        } else {
            stick_reticle(ui, "L STICK", "AXIS 0", lx, "AXIS 1", ly);
            ui.add_space(6.0);
            stick_reticle(ui, "R STICK", "AXIS 2", rx, "AXIS 3", ry);
            ui.add_space(6.0);
            diagnostics_label(ui, "Axes & triggers");
            axes_list(ui, view);
        }
    }

    fn controller_paddles_strip(&mut self, ui: &mut Ui, live: &GamepadLiveView) {
        let btn_width = 68.0_f32;
        let btn_height = 22.0_f32;
        let spacing = 8.0_f32;
        let total_width = 4.0 * btn_width + 3.0 * spacing;
        let pad = ((ui.available_width() - total_width) * 0.5).max(0.0);

        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            if pad > 0.0 {
                ui.add_space(pad);
            }
            for (name, code) in [
                ("Paddle 1", "BTN_TRIGGER_HAPPY1"),
                ("Paddle 2", "BTN_TRIGGER_HAPPY2"),
                ("Paddle 3", "BTN_TRIGGER_HAPPY3"),
                ("Paddle 4", "BTN_TRIGGER_HAPPY4"),
            ] {
                let is_pressed = live.button(code);
                let is_selected = self.selected == name;
                let (rect, resp) =
                    ui.allocate_exact_size(Vec2::new(btn_width, btn_height), Sense::click());
                let fill = if is_pressed {
                    Color32::from_rgba_unmultiplied(116, 162, 255, 120)
                } else if is_selected {
                    Color32::from_rgba_unmultiplied(116, 162, 255, 45)
                } else if resp.hovered() {
                    Color32::from_rgba_unmultiplied(116, 162, 255, 20)
                } else {
                    FIELD_BG
                };
                let stroke = if is_pressed {
                    Stroke::new(1.8_f32, Color32::WHITE)
                } else if is_selected {
                    Stroke::new(1.5_f32, ACCENT)
                } else if resp.hovered() {
                    Stroke::new(1.0_f32, Color32::from_rgb(220, 230, 245))
                } else {
                    Stroke::new(1.0_f32, BORDER)
                };
                ui.painter().rect_filled(rect, 4.0, fill);
                ui.painter()
                    .rect_stroke(rect, 4.0, stroke, StrokeKind::Inside);
                let text_color = if is_pressed {
                    Color32::WHITE
                } else if is_selected {
                    ACCENT
                } else {
                    TEXT_MUTED
                };
                ui.painter().text(
                    rect.center(),
                    Align2::CENTER_CENTER,
                    name,
                    FontId::proportional(9.5),
                    text_color,
                );
                if resp.clicked() {
                    self.selected = name;
                }
            }
        });
    }

    fn dpad_polygon(template: Template, rect: Rect, shape: HotspotShape) -> Option<Vec<Pos2>> {
        DpadVectorArtwork::global().polygon_for(template, rect, shape)
    }

    fn dpad_arrow(template: Template, rect: Rect, shape: HotspotShape) -> Option<[Pos2; 3]> {
        DpadVectorArtwork::global().arrow_for(template, rect, shape)
    }

    fn controller_map_body(&mut self, ui: &mut Ui, live: &GamepadLiveView) -> Option<Rect> {
        let template = self.template;
        let width = ui.available_width().min(560.0);
        let height = (width * 0.72).clamp(220.0, 340.0);
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
            let interact_rect = if hotspot.label {
                let well_radius = rect.height() * (37.5 / 403.0);
                hit.union(Rect::from_min_max(
                    Pos2::new(center.x - 30.0, center.y + well_radius),
                    Pos2::new(center.x + 30.0, center.y + well_radius + 20.0),
                ))
            } else {
                hit
            };
            let selected = self.selected == hotspot.control;
            if selected {
                selected_rect = Some(hit);
            }
            let response = ui.interact(
                interact_rect,
                ui.id().with(("hotspot", template.label(), hotspot.control)),
                Sense::click(),
            );
            let is_pressed = self.hotspot_pressed(hotspot, live);
            let (shape_fill, shape_stroke) = match (is_pressed, selected, response.hovered()) {
                (true, true, _) => (
                    Color32::from_rgba_unmultiplied(116, 162, 255, 120),
                    Stroke::new(2.5_f32, Color32::WHITE),
                ),
                (true, false, _) => (
                    Color32::from_rgba_unmultiplied(116, 162, 255, 90),
                    Stroke::new(2.0_f32, Color32::WHITE),
                ),
                (false, true, _) => (
                    Color32::from_rgba_unmultiplied(116, 162, 255, 45),
                    Stroke::new(2.0_f32, ACCENT),
                ),
                (false, false, true) => (
                    Color32::from_rgba_unmultiplied(116, 162, 255, 25),
                    Stroke::new(1.5_f32, Color32::from_rgb(220, 230, 245)),
                ),
                (false, false, false) => (
                    Color32::TRANSPARENT,
                    Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(116, 162, 255, 110)),
                ),
            };

            match hotspot.shape {
                HotspotShape::Circle => {
                    let r = (hotspot.w * rect.width()).min(hotspot.h * rect.width()) / 2.0;
                    if shape_fill != Color32::TRANSPARENT {
                        ui.painter().circle_filled(center, r, shape_fill);
                    }
                    ui.painter().circle_stroke(center, r, shape_stroke);
                    let is_stick = matches!(hotspot.control, "L STICK" | "R STICK" | "L3" | "R3");
                    if is_stick {
                        let cross_stroke = Stroke::new(
                            1.0_f32,
                            Color32::from_rgba_unmultiplied(116, 162, 255, 45),
                        );
                        ui.painter().line_segment(
                            [
                                Pos2::new(center.x - r * 0.75, center.y),
                                Pos2::new(center.x + r * 0.75, center.y),
                            ],
                            cross_stroke,
                        );
                        ui.painter().line_segment(
                            [
                                Pos2::new(center.x, center.y - r * 0.75),
                                Pos2::new(center.x, center.y + r * 0.75),
                            ],
                            cross_stroke,
                        );
                        let is_left = matches!(hotspot.control, "L STICK" | "L3");
                        let (sx, sy) = live.stick(is_left);
                        let puck_radius = (r * 0.32).clamp(3.5, 7.0);
                        let max_travel = (r - puck_radius - 1.0).max(1.0);
                        let puck_pos = Pos2::new(
                            center.x + sx.clamp(-1.0, 1.0) * max_travel,
                            center.y + sy.clamp(-1.0, 1.0) * max_travel,
                        );
                        let puck_color = if is_pressed {
                            Color32::WHITE
                        } else if sx.abs() > 0.05 || sy.abs() > 0.05 {
                            ACCENT
                        } else {
                            Color32::from_rgba_unmultiplied(116, 162, 255, 140)
                        };
                        ui.painter()
                            .circle_filled(puck_pos, puck_radius, puck_color);
                        ui.painter().circle_stroke(
                            puck_pos,
                            puck_radius,
                            Stroke::new(1.0_f32, Color32::WHITE),
                        );
                    }
                }
                HotspotShape::Rect { corner_radius } => {
                    let is_trigger = matches!(hotspot.control, "LT" | "RT" | "L2" | "R2");
                    let trigger_val = if is_trigger {
                        match hotspot.control {
                            "LT" | "L2" => live.trigger("ABS_Z", "BTN_TL2"),
                            "RT" | "R2" => live.trigger("ABS_RZ", "BTN_TR2"),
                            _ => 0.0,
                        }
                    } else {
                        0.0
                    };
                    let rect_radius: egui::CornerRadius = if is_trigger {
                        let top_r = (hit.width() * 0.5).round().clamp(6.0, 16.0) as u8;
                        let bot_r = (hit.width() * 0.26).round().clamp(4.0, 10.0) as u8;
                        egui::CornerRadius {
                            nw: top_r,
                            ne: top_r,
                            sw: bot_r,
                            se: bot_r,
                        }
                    } else {
                        (corner_radius as u8).into()
                    };
                    if shape_fill != Color32::TRANSPARENT {
                        ui.painter().rect_filled(hit, rect_radius, shape_fill);
                    }
                    if is_trigger && trigger_val > 0.01 {
                        let fill_h = hit.height() * trigger_val.clamp(0.0, 1.0);
                        let fill_rect =
                            Rect::from_min_max(Pos2::new(hit.min.x, hit.max.y - fill_h), hit.max);
                        let fill_radius = if trigger_val >= 0.88 {
                            rect_radius
                        } else {
                            let bot_r = (hit.width() * 0.26).round().clamp(4.0, 10.0) as u8;
                            egui::CornerRadius {
                                nw: 0,
                                ne: 0,
                                sw: bot_r,
                                se: bot_r,
                            }
                        };
                        ui.painter().rect_filled(
                            fill_rect,
                            fill_radius,
                            Color32::from_rgba_unmultiplied(116, 162, 255, 160),
                        );
                    }
                    ui.painter()
                        .rect_stroke(hit, rect_radius, shape_stroke, StrokeKind::Inside);
                    if is_trigger && trigger_val > 0.01 {
                        let pct_str = format!("{:.0}%", trigger_val * 100.0);
                        let font_id = FontId::monospace((hit.height() * 0.26).clamp(9.0, 11.0));
                        let galley = ui
                            .painter()
                            .layout_no_wrap(pct_str, font_id, Color32::WHITE);
                        let pill_rect = Rect::from_min_size(
                            Pos2::new(hit.max.x + 5.0, hit.center().y - galley.size().y * 0.5),
                            galley.size(),
                        )
                        .expand2(Vec2::new(3.0, 1.5));
                        ui.painter().rect_filled(
                            pill_rect,
                            3.0,
                            Color32::from_rgba_unmultiplied(16, 20, 26, 210),
                        );
                        ui.painter().rect_stroke(
                            pill_rect,
                            3.0,
                            Stroke::new(
                                1.0_f32,
                                Color32::from_rgba_unmultiplied(116, 162, 255, 120),
                            ),
                            StrokeKind::Inside,
                        );
                        ui.painter().galley(
                            Pos2::new(hit.max.x + 5.0, hit.center().y - galley.size().y * 0.5),
                            galley,
                            Color32::WHITE,
                        );
                    }
                }
                HotspotShape::DpadUp
                | HotspotShape::DpadDown
                | HotspotShape::DpadLeft
                | HotspotShape::DpadRight => {
                    if let Some(pts) = Self::dpad_polygon(template, rect, hotspot.shape) {
                        let scale = rect.width() / 441.0;
                        let art = DpadVectorArtwork::global();
                        let fill = if is_pressed {
                            Color32::from_rgba_unmultiplied(116, 162, 255, 72)
                        } else if selected {
                            Color32::from_rgba_unmultiplied(116, 162, 255, 30)
                        } else if response.hovered() {
                            Color32::from_rgba_unmultiplied(116, 162, 255, 20)
                        } else {
                            art.fill_color
                        };
                        let shape_stroke = if is_pressed {
                            Stroke::new((1.8 * scale).max(1.5), Color32::WHITE)
                        } else if selected {
                            Stroke::new((2.0 * scale).max(1.5), ACCENT)
                        } else if response.hovered() {
                            Stroke::new((1.5 * scale).max(1.2), Color32::from_rgb(220, 230, 245))
                        } else {
                            Stroke::new((art.stroke_width * scale).max(1.0), art.stroke_color)
                        };
                        ui.painter()
                            .add(egui::Shape::convex_polygon(pts, fill, shape_stroke));
                    }
                }
            }

            if let Some(arrow_pts) = Self::dpad_arrow(template, rect, hotspot.shape) {
                let arrow_color = if is_pressed {
                    Color32::WHITE
                } else if selected {
                    ACCENT
                } else if response.hovered() {
                    Color32::from_rgb(230, 240, 255)
                } else {
                    DpadVectorArtwork::global().arrow_color
                };
                ui.painter().add(egui::Shape::convex_polygon(
                    arrow_pts.to_vec(),
                    arrow_color,
                    Stroke::NONE,
                ));
            } else if hotspot.label {
                let well_radius = rect.height() * (37.5 / 403.0);
                let pos = Pos2::new(center.x, center.y + well_radius + 4.0);
                let stick_font = FontId::monospace((rect.width() * 0.022).clamp(9.0, 11.5));
                let text_color = if selected {
                    Color32::WHITE
                } else {
                    Color32::from_rgb(193, 215, 238)
                };
                let galley = ui.painter().layout_no_wrap(
                    hotspot.control.to_string(),
                    stick_font,
                    text_color,
                );
                let text_pos = Pos2::new(center.x - galley.size().x * 0.5, pos.y);
                let pill_rect =
                    Rect::from_min_size(text_pos, galley.size()).expand2(Vec2::new(4.0, 2.0));
                let pill_fill = if selected {
                    Color32::from_rgba_unmultiplied(28, 38, 54, 230)
                } else {
                    Color32::from_rgba_unmultiplied(16, 20, 26, 220)
                };
                let pill_stroke = if selected {
                    Stroke::new(1.5_f32, ACCENT)
                } else if response.hovered() {
                    Stroke::new(1.0_f32, Color32::from_rgb(220, 230, 245))
                } else {
                    Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(193, 215, 238, 70))
                };
                ui.painter().rect_filled(pill_rect, 3.0, pill_fill);
                ui.painter()
                    .rect_stroke(pill_rect, 3.0, pill_stroke, StrokeKind::Inside);
                ui.painter().galley(text_pos, galley, text_color);
            }
            if response.clicked() {
                self.selected = hotspot.control;
            }
        }
        selected_rect
    }

    fn hotspot_pressed(&self, hotspot: &Hotspot, live: &GamepadLiveView) -> bool {
        hotspot.is_pressed(live)
    }

    fn switch_template(&mut self, template: Template) {
        if template == self.template {
            return;
        }
        if self.selected.starts_with("Paddle") {
            self.template = template;
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
            .or(match self.selected {
                "Paddle 1" => Some("BTN_TRIGGER_HAPPY1"),
                "Paddle 2" => Some("BTN_TRIGGER_HAPPY2"),
                "Paddle 3" => Some("BTN_TRIGGER_HAPPY3"),
                "Paddle 4" => Some("BTN_TRIGGER_HAPPY4"),
                _ => None,
            })
    }

    fn inspector_body(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Selected")
                    .size(11.0)
                    .strong()
                    .color(TEXT_MUTED),
            );
            let code = self.selected_code();
            if let Some(code) = code {
                let map_path = binding_path(&self.context);
                let is_bound = get_path(&self.model, &map_path)
                    .and_then(Value::as_object)
                    .and_then(|map| map.get(code))
                    .is_some();
                let (status_text, status_color) = if is_bound {
                    ("bound", OK)
                } else {
                    ("unbound", TEXT_MUTED)
                };
                ui.label(
                    RichText::new(format!("({status_text} in {})", self.context))
                        .size(10.0)
                        .color(status_color),
                );
            } else {
                ui.label(RichText::new("(unmapped)").size(10.0).color(TEXT_MUTED));
            }
        });
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
        let actions = if self.context == "xmb" {
            XMB_ACTIONS
        } else {
            ACTIONS
        };
        ui.horizontal(|ui| {
            field_label(ui, "Action");
            ComboBox::from_id_salt("binding-action")
                .width(input_width(ui).clamp(90.0, 260.0))
                .selected_text(if next.is_empty() { "unbound" } else { &next })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut next, String::new(), "unbound");
                    for action in actions {
                        ui.selectable_value(&mut next, (*action).to_string(), *action);
                    }
                });
        });
        if self.context != "xmb" {
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Quick map:").size(9.5).color(TEXT_DIM));
                for (label, act) in [
                    ("Mouse L", "pointer.left"),
                    ("Mouse R", "pointer.right"),
                    ("Mouse M", "pointer.middle"),
                    ("M4", "pointer.back"),
                    ("M5", "pointer.forward"),
                    ("Scroll ▲", "pointer.scroll-up"),
                    ("Scroll ▼", "pointer.scroll-down"),
                ] {
                    let is_current = current == act;
                    let text = RichText::new(label).size(9.5).color(if is_current {
                        Color32::WHITE
                    } else {
                        TEXT_MUTED
                    });
                    let btn = egui::Button::new(text)
                        .fill(if is_current {
                            Color32::from_rgba_unmultiplied(116, 162, 255, 60)
                        } else {
                            FIELD_BG
                        })
                        .stroke(if is_current {
                            Stroke::new(1.0_f32, ACCENT)
                        } else {
                            Stroke::new(1.0_f32, BORDER)
                        })
                        .corner_radius(3.0);
                    if ui.add(btn).clicked() {
                        if is_current {
                            next = String::new();
                        } else {
                            next = act.to_string();
                        }
                    }
                }
            });
        }
        if next != current {
            if next.is_empty() {
                remove_map_key(&mut self.model, &map_path, code);
            } else {
                set_map_value(&mut self.model, &map_path, code, &next);
            }
        }
    }

    fn device_fields(&mut self, ui: &mut Ui) {
        self.show_exact_field_error(ui, "gamepad");
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
                    0.25,
                    3.0,
                    0.05,
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.speed",
                    "Speed",
                    0.0,
                    2400.0,
                    10.0,
                );
                self.f32_field(
                    &mut cols[0],
                    "gamepad.navi.pointer.max_step",
                    "Max step",
                    1.0,
                    96.0,
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
                    30.0,
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
            self.f32_field(ui, "gamepad.navi.pointer.curve", "Curve", 0.25, 3.0, 0.05);
            self.f32_field(ui, "gamepad.navi.pointer.speed", "Speed", 0.0, 2400.0, 10.0);
            self.f32_field(
                ui,
                "gamepad.navi.pointer.max_step",
                "Max step",
                1.0,
                96.0,
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
                30.0,
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
            .unwrap_or(0.08);
        let changed = field_row_path(
            ui,
            Some("gamepad.navi.pointer.trigger_deadzone"),
            "Pull deadzone",
            |ui| numeric_slider_f32(ui, &mut value, 0.0, max as f64, 0.005, ""),
        );
        if changed {
            set_json_path(
                &mut self.model,
                "gamepad.navi.pointer.trigger_deadzone",
                json!(value),
            );
        }
        if value > max as f64 {
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
            let mut pull = point.pull as f64;
            let mut output = point.output as f64;
            let pull_changed = numeric_slider_f32(
                ui,
                &mut pull,
                (deadzone + 0.001) as f64,
                0.999,
                0.005,
                " pull",
            );
            let output_changed = numeric_slider_f32(ui, &mut output, 0.0, 1.0, 0.005, " shape");
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
                    next[index].pull = (pull as f32).clamp(min_pull, max_pull);
                }
                next[index].output = (output as f32).clamp(previous_output, next_output);
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
            RichText::new("LT brake (decel) · RT gas (accel) · 1.0× baseline")
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
                    "LT decel {:.2}× ({:.0}% speed)",
                    decel_floor,
                    decel_floor * 100.0
                ))
                .monospace()
                .size(10.0)
                .color(WARN),
            );
            ui.label(
                RichText::new(format!(
                    "RT accel {:.2}× ({:.0}% speed)",
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

    fn accel_settings_group(&mut self, ui: &mut Ui, base: &str, title: &str) {
        mini_heading(ui, title);
        self.bool_field(ui, &format!("{base}.enabled"), "Enabled");
        self.f32_field(
            ui,
            &format!("{base}.decel_floor"),
            "Brake floor (min ×)",
            0.01,
            1.0,
            0.01,
        );
        self.f32_field(
            ui,
            &format!("{base}.boost_cap"),
            "Gas boost (max ×)",
            1.0,
            10.0,
            0.05,
        );
        self.f32_field(
            ui,
            &format!("{base}.smoothing_ms"),
            "Speed smoothing (ms)",
            0.0,
            500.0,
            1.0,
        );
    }

    fn accel_fields(&mut self, ui: &mut Ui) {
        let pointer = "gamepad.navi.pointer";
        if ui.available_width() >= 660.0 {
            ui.columns(3, |cols| {
                mini_heading(&mut cols[0], "Response shape & trigger conflict");
                self.trigger_deadzone_field(&mut cols[0]);
                self.f32_field(
                    &mut cols[0],
                    &format!("{pointer}.trigger_curve"),
                    "Response curve",
                    0.25,
                    3.0,
                    0.05,
                );
                self.combo_field(
                    &mut cols[0],
                    &format!("{pointer}.speed_socd"),
                    "When LT + RT are held",
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
                    "Layer shift threshold (not speed)",
                    0.0,
                    1.0,
                    0.01,
                );
                self.accel_settings_group(
                    &mut cols[1],
                    &format!("{pointer}.accel"),
                    "Pointer speed range",
                );
                self.accel_settings_group(
                    &mut cols[2],
                    &format!("{pointer}.scroll_accel"),
                    "Scroll speed range",
                );
            });
        } else {
            mini_heading(ui, "Response shape & trigger conflict");
            self.trigger_deadzone_field(ui);
            self.f32_field(
                ui,
                &format!("{pointer}.trigger_curve"),
                "Response curve",
                0.25,
                3.0,
                0.05,
            );
            self.combo_field(
                ui,
                &format!("{pointer}.speed_socd"),
                "When LT + RT are held",
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
                "Layer shift threshold (not speed)",
                0.0,
                1.0,
                0.01,
            );
            ui.add_space(4.0);
            self.accel_settings_group(ui, &format!("{pointer}.accel"), "Pointer speed range");
            ui.add_space(4.0);
            self.accel_settings_group(ui, &format!("{pointer}.scroll_accel"), "Scroll speed range");
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
        self.segmented_field(
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
            96.0,
            1.0,
        );
        self.f32_field(
            ui,
            "behavior.drag_threshold",
            "Drag threshold",
            0.0,
            96.0,
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
        let width = ui.available_width();
        if width >= 960.0 {
            let preview_width = (width * 0.44).clamp(420.0, 680.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(preview_width);
                    self.popup_preview_card(ui);
                });
                ui.add_space(10.0);
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    card(ui, |ui| self.popup_tabs_panel(ui));
                });
            });
        } else {
            self.popup_preview_card(ui);
            ui.add_space(8.0);
            card(ui, |ui| self.popup_tabs_panel(ui));
        }
    }

    fn popup_tabs_panel(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 6.0);
            for tab in PopupTab::ALL {
                let selected = self.popup_tab == tab;
                let response = ui.selectable_label(selected, tab.label());
                if response.clicked() {
                    self.popup_tab = tab;
                }
            }
        });
        ui.add_space(6.0);
        ui.separator();
        ui.add_space(6.0);
        match self.popup_tab {
            PopupTab::PanelPlacements => self.panel_placements_fields(ui),
            PopupTab::BehaviorPeekAnimation => self.behavior_peek_animation_fields(ui),
            PopupTab::Theme => self.style_fields(ui),
            PopupTab::Bars => self.bars_fields(ui),
        }
    }

    fn panel_placements_fields(&mut self, ui: &mut Ui) {
        section_card(ui, "Panel", |ui| self.popup_panel_fields(ui));
        section_card(ui, "Placements", |ui| self.placements_ui(ui));
    }

    fn behavior_peek_animation_fields(&mut self, ui: &mut Ui) {
        section_card(ui, "Behavior", |ui| self.behavior_fields(ui));
        section_card(ui, "Peek", |ui| self.popup_peek_fields(ui));
        section_card(ui, "Animation", |ui| self.popup_animation_fields(ui));
    }

    fn popup_panel_fields(&mut self, ui: &mut Ui) {
        self.show_exact_field_error(ui, "popup");
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                self.segmented_field(
                    &mut cols[0],
                    "popup.outputs",
                    "Outputs",
                    &["focused", "all", "combined"],
                );
                self.segmented_field(
                    &mut cols[0],
                    "popup.surface_mode",
                    "Surface",
                    &["fixed", "follow_active", "mirror"],
                );
                self.segmented_field(
                    &mut cols[0],
                    "popup.position",
                    "Position",
                    &["cursor", "fixed", "center", "anchored"],
                );
                self.length_field(&mut cols[0], "popup.width", "Width");
                self.length_field(&mut cols[0], "popup.height", "Height");
                self.slider_u64_field(
                    &mut cols[0],
                    "popup.follow_delay_ms",
                    "Follow delay ms",
                    0,
                    1000,
                    10,
                    " ms",
                );
                self.length_field(&mut cols[1], "popup.fixed_x", "Fixed X");
                self.length_field(&mut cols[1], "popup.fixed_y", "Fixed Y");
                self.length_field(&mut cols[1], "popup.canvas_width", "Canvas width");
                self.length_field(&mut cols[1], "popup.canvas_height", "Canvas height");
                self.bool_field(&mut cols[1], "popup.show_labels", "Labels");
                self.bool_field(&mut cols[1], "popup.show_titles", "Titles");
            });
        } else {
            self.segmented_field(
                ui,
                "popup.outputs",
                "Outputs",
                &["focused", "all", "combined"],
            );
            self.segmented_field(
                ui,
                "popup.surface_mode",
                "Surface",
                &["fixed", "follow_active", "mirror"],
            );
            self.segmented_field(
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
            self.slider_u64_field(
                ui,
                "popup.follow_delay_ms",
                "Follow delay ms",
                0,
                5000,
                10,
                " ms",
            );
            self.bool_field(ui, "popup.show_labels", "Labels");
            self.bool_field(ui, "popup.show_titles", "Titles");
        }
    }

    fn popup_peek_fields(&mut self, ui: &mut Ui) {
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                self.bool_field(&mut cols[0], "peek.enabled", "Enabled");
                self.slider_u64_field(
                    &mut cols[0],
                    "peek.duration_ms",
                    "Duration ms",
                    0,
                    5000,
                    10,
                    " ms",
                );
                self.bool_field(&mut cols[0], "peek.pointer_promote", "Pointer promote");
                self.bool_field(&mut cols[1], "peek.move_popup", "Border move");
                self.slider_u64_field(
                    &mut cols[1],
                    "peek.arm_grace_ms",
                    "Arm grace ms",
                    0,
                    5000,
                    10,
                    " ms",
                );
                self.slider_u64_field(
                    &mut cols[1],
                    "peek.demote_ms",
                    "Demote ms",
                    0,
                    5000,
                    10,
                    " ms",
                );
            });
        } else {
            self.bool_field(ui, "peek.enabled", "Enabled");
            self.slider_u64_field(ui, "peek.duration_ms", "Duration ms", 0, 5000, 10, " ms");
            self.bool_field(ui, "peek.pointer_promote", "Pointer promote");
            self.bool_field(ui, "peek.move_popup", "Border move");
            self.slider_u64_field(ui, "peek.arm_grace_ms", "Arm grace ms", 0, 5000, 10, " ms");
            self.slider_u64_field(ui, "peek.demote_ms", "Demote ms", 0, 5000, 10, " ms");
        }
    }

    fn popup_animation_fields(&mut self, ui: &mut Ui) {
        if ui.available_width() >= 480.0 {
            ui.columns(2, |cols| {
                self.bool_field(&mut cols[0], "animation.enabled", "Enabled");
                self.slider_f32_field(
                    &mut cols[0],
                    "animation.panel_ms",
                    "Panel ms",
                    0.0,
                    1000.0,
                    1.0,
                    " ms",
                );
                self.slider_f32_field(
                    &mut cols[0],
                    "animation.emphasis_ms",
                    "Emphasis ms",
                    0.0,
                    1000.0,
                    1.0,
                    " ms",
                );
                self.slider_f32_field(
                    &mut cols[1],
                    "animation.scroll_ms",
                    "Scroll ms",
                    0.0,
                    1000.0,
                    1.0,
                    " ms",
                );
                self.slider_f32_field(
                    &mut cols[1],
                    "animation.open_scale",
                    "Open scale",
                    0.1,
                    2.0,
                    0.01,
                    "×",
                );
            });
        } else {
            self.bool_field(ui, "animation.enabled", "Enabled");
            self.slider_f32_field(
                ui,
                "animation.panel_ms",
                "Panel ms",
                0.0,
                1000.0,
                1.0,
                " ms",
            );
            self.slider_f32_field(
                ui,
                "animation.emphasis_ms",
                "Emphasis ms",
                0.0,
                1000.0,
                1.0,
                " ms",
            );
            self.slider_f32_field(
                ui,
                "animation.scroll_ms",
                "Scroll ms",
                0.0,
                1000.0,
                1.0,
                " ms",
            );
            self.slider_f32_field(
                ui,
                "animation.open_scale",
                "Open scale",
                0.1,
                2.0,
                0.01,
                "×",
            );
        }
    }

    /// Fixed top panel for a pinned preview. It is a real panel, not prose:
    /// it renders the same live preview above the scrolling editor so the
    /// panel stays visible while the user edits and scrolls.
    fn pinned_preview_panel(&mut self, ctx: &egui::Context) {
        if !(self.section == Section::Popup && self.preview_pinned) {
            return;
        }
        egui::TopBottomPanel::top("pinned-preview")
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Live preview — pinned").size(12.5).strong());
                    ui.label(
                        RichText::new("stays on top while you scroll")
                            .size(10.5)
                            .color(TEXT_DIM),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if tt(ui.button("Unpin"), "ui.action.pin_preview").clicked() {
                            self.preview_pinned = false;
                        }
                    });
                });
                ui.add_space(4.0);
                let width = ui.available_width().max(220.0);
                self.live_xmb_preview(ui, Vec2::new(width, 300.0));
            });
    }

    fn popup_preview_card(&mut self, ui: &mut Ui) {
        section_card(ui, "Live preview", |ui| {
            ui.horizontal(|ui| {
                let label = if self.preview_pinned { "Unpin" } else { "Pin" };
                if tt(ui.small_button(label), "ui.action.pin_preview").clicked() {
                    self.preview_pinned = !self.preview_pinned;
                }
                ui.label(
                    RichText::new(if self.preview_pinned {
                        "Pinned — the preview stays on top while you scroll"
                    } else {
                        "Pin to keep the preview on top while you scroll"
                    })
                    .size(10.0)
                    .color(TEXT_MUTED),
                );
            });
            ui.add_space(6.0);
            if self.preview_pinned {
                ui.label(
                    RichText::new("Preview is pinned above.")
                        .size(10.5)
                        .color(TEXT_DIM),
                );
                return;
            }
            let available = ui.available_size();
            let wide = available.x >= 760.0;
            let preview_height = available.y.min(if wide { 360.0 } else { 300.0 }).max(220.0);
            let preview_width = if wide {
                (available.x * 0.62).clamp(280.0, 760.0)
            } else {
                available.x.max(220.0)
            };
            if wide {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(
                        Vec2::new(preview_width, preview_height),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_min_height(preview_height);
                            self.live_xmb_preview(ui, Vec2::new(preview_width, preview_height));
                        },
                    );
                    ui.add_space(14.0);
                    self.popup_preview_meta(ui);
                });
            } else {
                self.live_xmb_preview(ui, Vec2::new(preview_width, preview_height));
                ui.add_space(10.0);
                self.popup_preview_meta(ui);
            }
        });
    }

    fn popup_preview_meta(&self, ui: &mut Ui) {
        ui.vertical(|ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Current panel").size(12.0).strong());
            ui.add_space(4.0);
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
            ui.add_space(8.0);
            ui.label(
                RichText::new("Hover to highlight · click a window or workspace to focus it.")
                    .size(10.0)
                    .color(TEXT_MUTED),
            );
            ui.add_space(8.0);
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
        });
    }

    fn keyboard_section(&mut self, ui: &mut Ui) {
        self.section_header(ui);
        if self.keymap_at.elapsed() > Duration::from_secs(5) {
            self.reload_keymap();
        }
        section_card(ui, "Keymap", |ui| {
            let body = ui.available_size();
            self.keyboard_content(ui, body);
        });
    }

    fn style_fields(&mut self, ui: &mut Ui) {
        section_card(ui, "Theme", |ui| {
            self.theme_selector(ui);
        });
        section_card(ui, "Colors", |ui| {
            // Two columns need room for label + hex text + swatch per row;
            // below this the rows share one column instead of painting over
            // each other when the window is narrow or zoomed.
            if ui.available_width() >= 720.0 {
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
            self.segmented_field(
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
                        if matches!(path, "style.dim_alpha" | "style.urgent_glow_alpha") {
                            self.slider_f32_field(
                                &mut cols[column],
                                path,
                                label,
                                min,
                                max,
                                step,
                                "",
                            );
                        } else {
                            self.f32_field(&mut cols[column], path, label, min, max, step);
                        }
                    }
                });
            } else {
                for (path, label, min, max, step) in GEOMETRY_FIELDS.iter().copied() {
                    if matches!(path, "style.dim_alpha" | "style.urgent_glow_alpha") {
                        self.slider_f32_field(ui, path, label, min, max, step, "");
                    } else {
                        self.f32_field(ui, path, label, min, max, step);
                    }
                }
            }
            self.u64_field(ui, "style.icon_size", "Icon size", 1, 128);
            self.u64_field(ui, "style.icon_size_focused", "Focused icon size", 1, 128);
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
                let edge = self.model["bars"][index]["edge"].as_str().unwrap_or("top");
                let (max_width, max_height) = if matches!(edge, "left" | "right") {
                    (512, 8192)
                } else {
                    (8192, 512)
                };
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
                        self.u64_field(
                            &mut cols[0],
                            &format!("{base}.width"),
                            "Width",
                            1,
                            max_width.min(1920),
                        );
                        self.u64_field(
                            &mut cols[0],
                            &format!("{base}.height"),
                            "Height",
                            1,
                            max_height.min(512),
                        );
                        self.i64_field(&mut cols[0], &format!("{base}.margin"), "Margin", 0, 128);
                        self.i64_field(
                            &mut cols[0],
                            &format!("{base}.offset"),
                            "Offset",
                            -256,
                            256,
                        );
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
                            5_000,
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
                    self.u64_field(
                        ui,
                        &format!("{base}.width"),
                        "Width",
                        1,
                        max_width.min(1920),
                    );
                    self.u64_field(
                        ui,
                        &format!("{base}.height"),
                        "Height",
                        1,
                        max_height.min(512),
                    );
                    self.i64_field(ui, &format!("{base}.margin"), "Margin", 0, 128);
                    self.i64_field(ui, &format!("{base}.offset"), "Offset", -256, 256);
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
                        5_000,
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
            if tt(ui.button("Add bar"), "ui.action.add_bar").clicked() {
                let seed = self.output_seed("bars");
                if let Some(bars) = self.model["bars"].as_array_mut() {
                    bars.push(validation::default_bar(seed));
                }
            }
        });
    }

    /// Theme picker: a dropdown of the theme files beside the config when
    /// there is more than one to choose from, the free-text path field, and
    /// a `+` button that creates a new valid theme file and selects it.
    fn theme_selector(&mut self, ui: &mut Ui) {
        let candidates = theme_candidates(&self.config_path);
        if candidates.len() >= 2 {
            let current = get_path(&self.model, "theme")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let normalized = normalize_theme_path(&current, &self.config_path);
            let mut selected = if current.trim().is_empty() {
                String::new()
            } else {
                candidates
                    .iter()
                    .find(|candidate| {
                        normalize_theme_path(candidate, &self.config_path) == normalized
                    })
                    .cloned()
                    .unwrap_or_else(|| current.clone())
            };
            let old = selected.clone();
            field_row_path(ui, Some("theme"), "Theme", |ui| {
                let mut changed = false;
                ComboBox::from_id_salt("theme-selector")
                    .width(input_width(ui).clamp(90.0, 280.0))
                    .selected_text(if selected.is_empty() {
                        "(none)".to_string()
                    } else {
                        theme_display_name(&selected).to_string()
                    })
                    .show_ui(ui, |ui| {
                        changed |= ui
                            .selectable_value(&mut selected, String::new(), "(none)")
                            .changed();
                        for candidate in &candidates {
                            changed |= ui
                                .selectable_value(
                                    &mut selected,
                                    candidate.clone(),
                                    theme_display_name(candidate),
                                )
                                .changed();
                        }
                        if !old.is_empty() && !candidates.contains(&old) {
                            changed |= ui
                                .selectable_value(
                                    &mut selected,
                                    old.clone(),
                                    theme_display_name(&old),
                                )
                                .changed();
                        }
                    });
                changed
            });
            if selected != old {
                if selected.trim().is_empty() {
                    set_json_path(&mut self.model, "theme", Value::Null);
                } else {
                    set_json_path(&mut self.model, "theme", Value::String(selected));
                }
            }
        }
        self.text_field(ui, "theme", "Theme file");
        ui.horizontal(|ui| {
            if tt(ui.button("+ New theme"), "ui.action.new_theme").clicked() {
                match create_theme_file(&self.config_path, &self.model) {
                    Some(path) => {
                        set_json_path(&mut self.model, "theme", Value::String(path));
                    }
                    None => {
                        self.error = "Could not create a theme file beside the config".to_string();
                    }
                }
            }
            ui.label(
                RichText::new("Creates themes/custom-N.toml with valid defaults and selects it.")
                    .size(10.0)
                    .color(TEXT_MUTED),
            );
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
            // An empty theme means "no theme", not an invalid empty path.
            if path == "theme" && value.trim().is_empty() {
                set_json_path(&mut self.model, path, Value::Null);
            } else {
                set_json_path(&mut self.model, path, Value::String(value));
            }
        }
        self.show_field_error(ui, path);
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
            numeric_slider_f32(ui, &mut value, min, max, speed, "")
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
        self.show_field_error(ui, path);
    }

    #[allow(clippy::too_many_arguments)]
    fn slider_f32_field(
        &mut self,
        ui: &mut Ui,
        path: &str,
        label: &str,
        min: f64,
        max: f64,
        step: f64,
        suffix: &str,
    ) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let changed = field_row_path(ui, Some(path), label, |ui| {
            numeric_slider_f32(ui, &mut value, min, max, step, suffix)
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
        self.show_field_error(ui, path);
    }

    fn u64_field(&mut self, ui: &mut Ui, path: &str, label: &str, min: u64, max: u64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let changed = field_row_path(ui, Some(path), label, |ui| {
            numeric_slider_u64(ui, &mut value, min, max, 1.0, "")
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
        self.show_field_error(ui, path);
    }

    #[allow(clippy::too_many_arguments)]
    fn slider_u64_field(
        &mut self,
        ui: &mut Ui,
        path: &str,
        label: &str,
        min: u64,
        max: u64,
        step: u64,
        suffix: &str,
    ) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let changed = field_row_path(ui, Some(path), label, |ui| {
            numeric_slider_u64(ui, &mut value, min, max, step as f64, suffix)
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
        self.show_field_error(ui, path);
    }

    fn i64_field(&mut self, ui: &mut Ui, path: &str, label: &str, min: i64, max: i64) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let changed = field_row_path(ui, Some(path), label, |ui| {
            numeric_slider_i64(ui, &mut value, min, max, 1.0, "")
        });
        if changed {
            set_json_path(&mut self.model, path, json!(value));
        }
        self.show_field_error(ui, path);
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
        self.show_field_error(ui, path);
    }

    fn segmented_field(&mut self, ui: &mut Ui, path: &str, label: &str, options: &[&str]) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let old = value.clone();
        let changed = field_row_path(ui, Some(path), label, |ui| {
            let mut changed = false;
            ui.horizontal_wrapped(|ui| {
                for option in options {
                    changed |= ui
                        .selectable_value(
                            &mut value,
                            (*option).to_string(),
                            segmented_option_label(option),
                        )
                        .changed();
                }
            });
            changed
        });
        if changed || value != old {
            set_json_path(&mut self.model, path, Value::String(value));
        }
        self.show_field_error(ui, path);
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
        self.show_field_error(ui, path);
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
        self.show_field_error(ui, path);
    }

    fn length_reference_extent(&self, path: &str) -> Option<f64> {
        let live = self.live.lock().ok()?.clone();
        let snapshot = snapshot_from_json(&live.snapshot)?;
        let output = snapshot
            .focused_output()
            .and_then(|name| snapshot.outputs.get(name))?;
        if path.contains("height") || path.contains("fixed_y") {
            Some(output.height.max(1) as f64)
        } else {
            Some(output.width.max(1) as f64)
        }
    }

    fn length_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let bounds = config_length_bounds(path).expect("length editor path has daemon bounds");
        let source = get_path(&self.model, path).unwrap_or(&Value::Null);
        let (mut unit, mut text, parsed) = parse_length_value(source);
        let mut number = parsed.unwrap_or(0.0);
        let reference_extent = self.length_reference_extent(path);
        let changed = field_row_path(ui, Some(path), label, |ui| {
            ui.horizontal(|ui| {
                let mut changed;
                if parsed.is_some() {
                    let (min, max) = length_slider_bounds(bounds, unit);
                    changed = numeric_slider_f32(ui, &mut number, min, max, 1.0, "");
                    if changed {
                        text = number.to_string();
                    }
                } else {
                    changed = ui
                        .add(egui::TextEdit::singleline(&mut text).desired_width(120.0))
                        .changed();
                }
                let next = match unit {
                    LengthUnit::Pixels => LengthUnit::Percent,
                    LengthUnit::Percent => LengthUnit::Pixels,
                };
                if ui
                    .small_button(unit.label())
                    .on_hover_text(format!("Switch to {}", next.label()))
                    .clicked()
                {
                    if parsed.is_some() {
                        (unit, number) =
                            toggle_length_value(unit, number, reference_extent, bounds);
                        text = number.to_string();
                    } else {
                        unit = next;
                    }
                    changed = true;
                }
                changed
            })
            .inner
        });
        if changed {
            set_json_path(&mut self.model, path, format_length_value(unit, &text));
        }
        self.show_field_error(ui, path);
    }

    fn color_field(&mut self, ui: &mut Ui, path: &str, label: &str) {
        let mut value = get_path(&self.model, path)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let changed = field_row_path(ui, Some(path), label, |ui| {
            // Fixed compact text width: the swatch shares the row, and an
            // adaptive width lets the row overflow narrow columns and paint
            // over the neighbouring column when zoomed.
            let text_width = 132.0_f32.min(ui.available_width()).max(64.0);
            let mut changed = ui
                .add(egui::TextEdit::singleline(&mut value).desired_width(text_width))
                .changed();
            // Interactive swatch: picking a colour writes it back as hex,
            // typing hex updates the swatch on the next frame.
            let mut swatch = style_color(&value);
            if ui
                .color_edit_button_srgba(&mut swatch)
                .on_hover_text("Pick a colour")
                .changed()
            {
                value = color_to_hex(swatch);
                changed = true;
            }
            changed
        });
        if changed {
            set_json_path(&mut self.model, path, Value::String(value));
        }
        self.show_field_error(ui, path);
    }

    /// Context tabs (XMB, Navi base and every hold layer) plus the bindings
    /// list for the active context. Layer tabs also expose the hold buttons and
    /// axes that define the layer, so all controller mapping lives in one card
    /// without a separate layers block. Returns the bound row's rect so the
    /// card can link it to the controller.
    fn merged_bindings(&mut self, ui: &mut Ui) -> Option<Rect> {
        let target = self.selected_code();
        let mut rect = None;

        let layers: Vec<String> = self.model["gamepad"]["navi"]["layers"]
            .as_object()
            .map(|layers| layers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        if self.context != "base"
            && self.context != "xmb"
            && !layers.iter().any(|name| name == &self.context)
        {
            self.context = "base".to_string();
        }

        ui.horizontal(|ui| {
            if ui.selectable_label(self.context == "xmb", "XMB").clicked() {
                self.context = "xmb".to_string();
            }
            if ui
                .selectable_label(self.context == "base", "Navi base")
                .clicked()
            {
                self.context = "base".to_string();
            }
            for name in &layers {
                if ui.selectable_label(self.context == *name, name).clicked() {
                    self.context = name.clone();
                }
            }
            if let Some(name) = self.add_layer_button(ui) {
                self.context = name;
            }
        });
        ui.add_space(4.0);
        if self.context == "xmb" {
            rect = self.bindings_ui(ui, "gamepad.buttons", XMB_ACTIONS, "XMB buttons", target);
        } else if self.context == "base" {
            rect = self.bindings_ui(ui, "gamepad.navi.bindings", ACTIONS, "Navi base", target);
        } else {
            let name = self.context.clone();
            self.layer_editor(ui, &name, target);
        }
        rect
    }

    /// Seed a fresh layer object with an unused name; returns the name so the
    /// caller can jump to its tab.
    fn add_layer_object(&mut self, chord: bool) -> Option<String> {
        let layers = ensure_object_path(&mut self.model, "gamepad.navi.layers");
        let seed: fn() -> Value = if chord {
            validation::default_chord_layer
        } else {
            validation::default_layer
        };
        let prefix = if chord { "chord" } else { "layer" };
        let mut name = prefix.to_string();
        let mut suffix = 1;
        while layers.contains_key(&name) {
            name = format!("{prefix}{suffix}");
            suffix += 1;
        }
        layers.insert(name.clone(), seed());
        Some(name)
    }

    fn add_layer_button(&mut self, ui: &mut Ui) -> Option<String> {
        let mut added = None;
        if tt(ui.add(egui::Button::new("+ Layer")), "ui.action.add_layer").clicked() {
            added = self.add_layer_object(false);
        }
        if ui
            .button("+ Chord")
            .on_hover_text("Create a valid layer holding Start, then bind another button")
            .clicked()
        {
            added = self.add_layer_object(true);
        }
        added
    }

    fn next_binding_entry<'a>(
        &self,
        path: &str,
        actions: &[&'a str],
        used: &[String],
    ) -> Option<(&'static str, &'a str)> {
        next_binding_entry(self.template, self.selected, path, actions, used)
    }

    fn default_action_for_code<'a>(&self, path: &str, actions: &[&'a str], code: &str) -> &'a str {
        default_action_for_code(path, actions, code)
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
        self.show_errors_for_prefix(ui, path);
        let used = entries
            .iter()
            .map(|(code, _)| code.clone())
            .collect::<Vec<_>>();
        let candidate = self.next_binding_entry(path, actions, &used);
        ui.horizontal(|ui| {
            let can_add = candidate.is_some();
            let add_btn = ui.add_enabled(can_add, egui::Button::new("Add binding"));
            let response = tt(add_btn, "ui.action.add_binding");
            if response.clicked()
                && let Some((code, action)) = candidate
            {
                set_map_value(&mut self.model, path, code, action);
                if let Some(control) = self
                    .template
                    .hotspots()
                    .iter()
                    .find(|h| h.code == Some(code))
                    .map(|h| h.control)
                {
                    self.selected = control;
                }
            }
            if !can_add {
                response.on_disabled_hover_text("All standard buttons are bound in this map");
            }
            if let Some(code) = button_menu(ui, "▾", &used) {
                let action = self.default_action_for_code(path, actions, code);
                set_map_value(&mut self.model, path, code, action);
                if let Some(control) = self
                    .template
                    .hotspots()
                    .iter()
                    .find(|h| h.code == Some(code))
                    .map(|h| h.control)
                {
                    self.selected = control;
                }
            }
        });
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
        let mut row_interacted = false;
        let selected = highlight == Some(code);
        // Register the row's background target before creating its children.
        // Egui resolves overlapping widgets in creation order, so the later
        // combo boxes and remove button remain above this target.
        let row_rect = Rect::from_min_size(
            ui.next_widget_position(),
            Vec2::new(ui.available_width(), ui.spacing().interact_size.y + 4.0),
        );
        let row_response = ui.interact(
            row_rect,
            egui::Id::new(("binding-row", path, code)),
            Sense::click(),
        );
        row_interacted |= row_response.clicked();
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
                    let code_response = ComboBox::from_id_salt(("binding-code", path, code))
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
                    row_interacted |= code_response.response.clicked() || code_changed;
                    let mut action_changed = false;
                    let action_response = ComboBox::from_id_salt(("binding", path, code))
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
                    row_interacted |= action_response.response.clicked() || action_changed;
                    remove = ui
                        .small_button("x")
                        .on_hover_text("Remove binding")
                        .clicked();
                    row_interacted |= remove;
                    code_changed || action_changed
                })
                .inner
            });
        // Do not call `frame.response.interact` here. `Frame::show` returns a
        // response after its children have been created, and `interact` adds
        // another full-row widget at that point. Egui gives the last widget
        // priority, so that invisible overlay consumed clicks intended for
        // both combo boxes and the remove button. Child responses above keep
        // row selection without covering those controls.
        if row_interacted
            && let Some(control) = self
                .template
                .hotspots()
                .iter()
                .find(|h| h.code == Some(code))
                .map(|h| h.control)
        {
            self.selected = control;
        }
        if remove {
            remove_map_key(&mut self.model, path, code);
        } else if next_code != code {
            remove_map_key(&mut self.model, path, code);
            set_map_value(&mut self.model, path, &next_code, &next_action);
            if let Some(control) = self
                .template
                .hotspots()
                .iter()
                .find(|h| h.code == Some(&next_code))
                .map(|h| h.control)
            {
                self.selected = control;
            }
        } else if frame.inner || next_action != action {
            set_map_value(&mut self.model, path, code, &next_action);
        }
        selected.then_some(frame.response.rect)
    }

    /// A single hold layer: identity + delete, hold buttons and axes, and its
    /// bindings. Shown under the layer's tab in `merged_bindings`.
    fn layer_editor(&mut self, ui: &mut Ui, name: &str, target: Option<&str>) {
        let mut remove_layer = None;
        let path = format!("gamepad.navi.layers.{name}");
        ui.horizontal(|ui| {
            ui.label(RichText::new(name).monospace().size(11.5).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Delete").clicked() {
                    remove_layer = Some(name.to_string());
                    self.context = "base".to_string();
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
                if numeric_slider_f32(ui, &mut threshold, 0.0, 1.0, 0.001, "") {
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
            self.show_field_error(ui, &axis_path);
        }
        if let Some(index) = remove_axis
            && let Some(axes) =
                self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"].as_array_mut()
        {
            axes.remove(index);
        }
        if tt(ui.button("Add hold axis"), "ui.action.add_hold_axis").clicked() {
            let used_axes = self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"]
                .as_array()
                .map(|axes| {
                    axes.iter()
                        .filter_map(|axis| axis.get("axis").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let axis = ["ABS_Z", "ABS_RZ", "ABS_X", "ABS_Y", "ABS_RX", "ABS_RY"]
                .into_iter()
                .find(|candidate| !used_axes.iter().any(|used| used == candidate))
                .unwrap_or("ABS_Z");
            if let Some(axes) =
                self.model["gamepad"]["navi"]["layers"][&name]["hold_axes"].as_array_mut()
            {
                axes.push(json!({"axis": axis, "threshold": 0.35}));
            }
        }
        self.show_exact_field_error(ui, &path);
        self.bindings_ui(ui, &format!("{path}.bindings"), ACTIONS, "Bindings", target);
        if let Some(name) = remove_layer
            && let Some(layers) = self.model["gamepad"]["navi"]["layers"].as_object_mut()
        {
            layers.remove(&name);
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
                    self.segmented_field(
                        &mut cols[0],
                        &format!("{base}.x"),
                        "X",
                        &["left", "center", "right"],
                    );
                    self.segmented_field(
                        &mut cols[0],
                        &format!("{base}.y"),
                        "Y",
                        &["top", "center", "bottom"],
                    );
                    self.length_field(&mut cols[1], &format!("{base}.margin"), "Margin");
                });
            } else {
                self.text_field(ui, &format!("{base}.output"), "Output");
                self.segmented_field(ui, &format!("{base}.x"), "X", &["left", "center", "right"]);
                self.segmented_field(ui, &format!("{base}.y"), "Y", &["top", "center", "bottom"]);
                self.length_field(ui, &format!("{base}.margin"), "Margin");
            }
        }
        if let Some(index) = remove
            && let Some(items) = self.model["popup"]["placements"].as_array_mut()
        {
            items.remove(index);
        }
        if tt(ui.button("Add placement"), "ui.action.add_placement").clicked() {
            let seed = self.output_seed("popup.placements");
            if let Some(items) = self.model["popup"]["placements"].as_array_mut() {
                items.push(validation::default_placement(seed));
            }
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
}

// ---------------------------------------------------------------------------
// Live controller and keyboard surfaces
// ---------------------------------------------------------------------------

/// Controller button and axis state aggregated from `niri-xmb status`.
#[derive(Default, Clone)]
struct GamepadLiveView {
    buttons: BTreeSet<String>,
    axes: BTreeMap<String, f32>,
    connected: usize,
    device_name: String,
    device_path: String,
    preset: String,
    vendor: u16,
    product: u16,
}

impl GamepadLiveView {
    fn from_status(status: &Value) -> Self {
        let connected = status["gamepad"]["connected"]
            .as_array()
            .map_or(0, Vec::len);
        let first = status["gamepad"]["connected"]
            .as_array()
            .and_then(|devices| devices.first())
            .cloned()
            .unwrap_or(Value::Null);
        let mut view = Self {
            connected,
            device_name: first["name"].as_str().unwrap_or("").to_string(),
            device_path: first["path"].as_str().unwrap_or("").to_string(),
            preset: first["preset"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| {
                    status["gamepad"]["preset"]
                        .as_str()
                        .unwrap_or("-")
                        .to_string()
                }),
            vendor: first["vendor"].as_u64().unwrap_or(0) as u16,
            product: first["product"].as_u64().unwrap_or(0) as u16,
            ..Self::default()
        };
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

    fn stick(&self, left: bool) -> (f32, f32) {
        if left {
            (self.dn_axis("ABS_X"), self.dn_axis("ABS_Y"))
        } else {
            (self.dn_axis("ABS_RX"), self.dn_axis("ABS_RY"))
        }
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

impl StudioApp {
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

    fn keyboard_content(&mut self, ui: &mut Ui, body: Vec2) {
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
            // Wide layout: board on the left, bind table on the right. The board
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
                let response =
                    ui.interact(cap, ui.id().with(("keyboard-key", *id)), Sense::click());
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

/// Largest 1u key edge. Without the cap a wide layout stretches one key across
/// the panel and the board stops reading as a keyboard.
const KEYBOARD_MAX_KEY: f32 = 40.0;
/// Smallest 1u key edge that still reads as a keyboard in a short layout.
const KEYBOARD_MIN_KEY: f32 = 9.0;

fn keyboard_legend_height(available: Vec2) -> f32 {
    let _ = available;
    0.0
}

/// Fit the 15x6 unit board into the page, bounded by both dimensions so keys
/// stay square however the splitter is dragged.
fn keyboard_cell(available: Vec2) -> f32 {
    ((available.x - 12.0) / 15.0)
        .min((available.y - keyboard_legend_height(available) - 12.0) / 6.0)
        .clamp(KEYBOARD_MIN_KEY, KEYBOARD_MAX_KEY)
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

fn diagnostics_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(10.0).strong().color(TEXT_MUTED));
    ui.add_space(4.0);
}

fn button_cell(ui: &mut Ui, label: &str, pressed: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(86.0, 36.0), Sense::hover());
    let fill = if pressed {
        Color32::from_rgba_unmultiplied(116, 162, 255, 70)
    } else {
        FIELD_BG
    };
    ui.painter().rect_filled(rect, 5.0, fill);
    ui.painter().rect_stroke(
        rect,
        5.0,
        Stroke::new(1.0_f32, if pressed { ACCENT } else { BORDER }),
        StrokeKind::Inside,
    );
    ui.painter().text(
        rect.min + Vec2::new(6.0, 4.0),
        Align2::LEFT_TOP,
        label,
        FontId::proportional(8.5),
        if pressed { TEXT } else { TEXT_MUTED },
    );
    ui.painter().text(
        rect.min + Vec2::new(6.0, 18.0),
        Align2::LEFT_TOP,
        if pressed { "1.00" } else { "0.00" },
        FontId::monospace(10.0),
        if pressed { Color32::WHITE } else { TEXT_DIM },
    );
}

fn stick_reticle(ui: &mut Ui, title: &str, x_label: &str, x: f32, y_label: &str, y: f32) {
    ui.vertical(|ui| {
        diagnostics_label(ui, title);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                for (label, value) in [(x_label, x), (y_label, y)] {
                    ui.label(RichText::new(label).monospace().size(8.5).color(TEXT_DIM));
                    ui.label(
                        RichText::new(format!("{value:.5}"))
                            .monospace()
                            .size(10.5)
                            .color(TEXT),
                    );
                    ui.add_space(4.0);
                }
            });
            ui.add_space(8.0);
            let size = 108.0;
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
            let center = rect.center();
            let radius = size / 2.0 - 2.0;
            ui.painter()
                .circle_stroke(center, radius, Stroke::new(1.0_f32, BORDER));
            ui.painter().line_segment(
                [
                    Pos2::new(center.x - radius, center.y),
                    Pos2::new(center.x + radius, center.y),
                ],
                Stroke::new(1.0_f32, BORDER),
            );
            ui.painter().line_segment(
                [
                    Pos2::new(center.x, center.y - radius),
                    Pos2::new(center.x, center.y + radius),
                ],
                Stroke::new(1.0_f32, BORDER),
            );
            let dot = Pos2::new(
                center.x + x.clamp(-1.0, 1.0) * radius,
                center.y + y.clamp(-1.0, 1.0) * radius,
            );
            ui.painter().circle_filled(dot, 4.0, ACCENT);
            ui.painter()
                .circle_stroke(dot, 4.0, Stroke::new(1.0_f32, Color32::WHITE));
        });
    });
}

/// Axes shown in the telemetry panel in stable order.
///
/// Xbox/evdev controllers expose sticks on `ABS_X/Y` + `ABS_RX/RY`, analog
/// triggers on `ABS_Z/RZ`, and the D-pad hat on `ABS_HAT0X/Y`. Missing entries
/// read as neutral `0.0` so an idle-but-connected controller still shows its
/// full control set instead of hiding; any extra axes from other hardware
/// append after, sorted.
const EXPECTED_AXES: &[&str] = &[
    "ABS_X",
    "ABS_Y",
    "ABS_RX",
    "ABS_RY",
    "ABS_Z",
    "ABS_RZ",
    "ABS_HAT0X",
    "ABS_HAT0Y",
];

fn telemetry_display_axes(view: &GamepadLiveView) -> Vec<(String, f32)> {
    let mut rows = Vec::with_capacity(EXPECTED_AXES.len() + view.axes.len());
    for axis in EXPECTED_AXES {
        rows.push((
            axis.to_string(),
            view.axes.get(*axis).copied().unwrap_or(0.0),
        ));
    }
    let mut extra: Vec<(String, f32)> = view
        .axes
        .iter()
        .filter(|(axis, _)| !EXPECTED_AXES.contains(&axis.as_str()))
        .map(|(axis, value)| (axis.clone(), *value))
        .collect();
    extra.sort_by(|a, b| a.0.cmp(&b.0));
    rows.extend(extra);
    rows
}

fn axes_list(ui: &mut Ui, view: &GamepadLiveView) {
    for (axis, value) in telemetry_display_axes(view) {
        let trigger = matches!(axis.as_str(), "ABS_Z" | "ABS_RZ");
        ui.horizontal(|ui| {
            ui.label(RichText::new(axis).monospace().size(9.0).color(TEXT_DIM));
            let width = (ui.available_width() - 64.0).clamp(70.0, 190.0);
            let (bar, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
            ui.painter().rect_filled(bar, 3.0, FIELD_BG);
            if trigger {
                ui.painter().rect_filled(
                    Rect::from_min_size(
                        bar.min,
                        Vec2::new(bar.width() * value.clamp(0.0, 1.0), bar.height()),
                    ),
                    3.0,
                    if value > 0.05 { ACCENT } else { BORDER },
                );
            } else {
                let center_x = bar.center().x;
                let dot_x = center_x + value.clamp(-1.0, 1.0) * bar.width() / 2.0;
                ui.painter().line_segment(
                    [
                        Pos2::new(center_x, bar.min.y),
                        Pos2::new(center_x, bar.max.y),
                    ],
                    Stroke::new(1.0_f32, BORDER),
                );
                ui.painter().rect_filled(
                    Rect::from_min_max(
                        Pos2::new(center_x.min(dot_x), bar.min.y),
                        Pos2::new(center_x.max(dot_x), bar.max.y),
                    ),
                    3.0,
                    if value.abs() > 0.05 { ACCENT } else { BORDER },
                );
            }
            ui.label(
                RichText::new(format!("{value:+.3}"))
                    .monospace()
                    .size(9.0)
                    .color(TEXT_DIM),
            );
        });
    }
    for (label, axis, button) in [("LT", "ABS_Z", "BTN_TL2"), ("RT", "ABS_RZ", "BTN_TR2")] {
        let value = view.trigger(axis, button);
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).monospace().size(9.0).color(TEXT_DIM));
            ui.label(
                RichText::new(format!("{:>3.0}%", value * 100.0))
                    .monospace()
                    .size(9.0)
                    .color(if value > 0.05 { TEXT } else { TEXT_DIM }),
            );
        });
    }
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
        let (undo, redo, save) = ctx.input_mut(|input| {
            let ctrl = input.modifiers.matches_logically(Modifiers::CTRL);
            (
                ctrl && !input.modifiers.shift && input.consume_key(Modifiers::CTRL, Key::Z),
                ctrl && (input.consume_key(Modifiers::CTRL, Key::Y)
                    || (input.modifiers.shift && input.consume_key(Modifiers::CTRL, Key::Z))),
                ctrl && input.consume_key(Modifiers::CTRL, Key::S),
            )
        });
        if redo {
            self.redo();
        } else if undo {
            self.undo();
        } else if save && self.dirty() {
            self.save(true);
        }
        self.refresh_validation();
        self.ensure_textures(ctx);
        self.command_bar(ctx);
        self.action_bar(ctx);
        if ctx.screen_rect().width() >= 700.0 {
            self.nav_rail(ctx);
        } else {
            self.nav_strip(ctx);
        }
        self.pinned_preview_panel(ctx);
        self.content(ctx);
        self.sync_history();
        // Live frames wake egui from the stream thread as they arrive.
        // This slow tick only ages the "last frame" readout and covers the
        // popup/keyboard views, which have no pushed feed of their own.
        if matches!(
            self.section,
            Section::Controller | Section::Popup | Section::Keyboard
        ) {
            ctx.request_repaint_after(Duration::from_millis(500));
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
///
/// Labels are right-aligned in a fixed-width box so the inputs of
/// consecutive rows start at the same x position. (`Ui::add_sized` centers
/// its content, which leaves both label edges ragged.)
fn field_row_path(
    ui: &mut Ui,
    path: Option<&str>,
    label: &str,
    add: impl FnOnce(&mut Ui) -> bool,
) -> bool {
    let label = path.map_or(label, |path| i18n::label_for_path(path, label));
    let label_width = stable_label_width(ui);
    ui.horizontal(|ui| {
        let label_response = right_label(ui, label_width, label);
        if let Some(hover) = i18n::hover_for(path, label) {
            label_response.on_hover_text(hover);
        }
        add(ui)
    })
    .inner
}

/// A compact slider paired with an unrestricted numeric editor. The slider
/// covers the useful everyday range; typing remains available for larger or
/// more precise values accepted by the daemon validator.
fn numeric_slider_f32(
    ui: &mut Ui,
    value: &mut f64,
    min: f64,
    max: f64,
    step: f64,
    suffix: &str,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        let width = numeric_slider_width(ui, suffix);
        ui.add_sized(
            [width, 20.0],
            egui::Slider::new(value, min..=max)
                .step_by(step)
                .show_value(false),
        );
        ui.add(
            egui::DragValue::new(value)
                .speed(step)
                .clamp_existing_to_range(false),
        );
        static_unit_label(ui, suffix);
    });
    *value != before
}

fn numeric_slider_u64(
    ui: &mut Ui,
    value: &mut u64,
    min: u64,
    max: u64,
    step: f64,
    suffix: &str,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        let width = numeric_slider_width(ui, suffix);
        ui.add_sized(
            [width, 20.0],
            egui::Slider::new(value, min..=max)
                .step_by(step)
                .show_value(false),
        );
        ui.add(
            egui::DragValue::new(value)
                .speed(step)
                .clamp_existing_to_range(false),
        );
        static_unit_label(ui, suffix);
    });
    *value != before
}

fn numeric_slider_i64(
    ui: &mut Ui,
    value: &mut i64,
    min: i64,
    max: i64,
    step: f64,
    suffix: &str,
) -> bool {
    let before = *value;
    ui.horizontal(|ui| {
        let width = numeric_slider_width(ui, suffix);
        ui.add_sized(
            [width, 20.0],
            egui::Slider::new(value, min..=max)
                .step_by(step)
                .show_value(false),
        );
        ui.add(
            egui::DragValue::new(value)
                .speed(step)
                .clamp_existing_to_range(false),
        );
        static_unit_label(ui, suffix);
    });
    *value != before
}

fn static_unit_label(ui: &mut Ui, suffix: &str) {
    let unit = suffix.trim();
    if !unit.is_empty() {
        ui.label(RichText::new(unit).color(TEXT_MUTED));
    }
}

fn numeric_slider_width(ui: &Ui, suffix: &str) -> f32 {
    let unit_reserve = if suffix.trim().is_empty() { 0.0 } else { 24.0 };
    (ui.available_width() - 84.0 - unit_reserve).max(64.0)
}

/// Attach the catalog hover text for a `ui.*` key to a widget response.
fn tt(response: egui::Response, key: &str) -> egui::Response {
    match i18n::hover_for_key(key) {
        Some(hover) => response.on_hover_text(hover),
        None => response,
    }
}

/// Draw the history controls as vector icons instead of relying on a font's
/// Unicode arrow coverage. This keeps the buttons visible across systems.
fn history_icon_button(ui: &mut Ui, enabled: bool, redo: bool, key: &str) -> egui::Response {
    let response = ui.add_enabled(
        enabled,
        egui::Button::new("").min_size(Vec2::new(28.0, 24.0)),
    );
    let rect = response.rect.shrink2(Vec2::new(7.0, 5.0));
    let color = if enabled { TEXT } else { TEXT_DIM };
    let stroke = Stroke::new(1.7_f32, color);
    let center = rect.center();
    let radius = (rect.width().min(rect.height()) * 0.48).max(3.0);
    let arc_rect = Rect::from_center_size(
        Pos2::new(center.x, center.y),
        Vec2::new(radius * 2.0, radius * 1.65),
    );
    let start = -std::f32::consts::FRAC_PI_2;
    let sweep = std::f32::consts::PI * 1.5;
    let mut points = Vec::with_capacity(25);
    for index in 0..=24 {
        let angle = start + sweep * index as f32 / 24.0;
        let unit = Vec2::new(angle.cos(), angle.sin());
        let point = Pos2::new(
            if redo {
                arc_rect.center().x - unit.x * arc_rect.width() * 0.5
            } else {
                arc_rect.center().x + unit.x * arc_rect.width() * 0.5
            },
            arc_rect.center().y + unit.y * arc_rect.height() * 0.5,
        );
        points.push(point);
    }
    ui.painter().add(egui::Shape::line(points.clone(), stroke));
    let tip = points[points.len() - 1];
    let direction = if redo { 1.0 } else { -1.0 };
    ui.painter()
        .line_segment([tip, tip + Vec2::new(-direction * 4.0, -3.0)], stroke);
    ui.painter()
        .line_segment([tip, tip + Vec2::new(-direction * 4.0, 3.0)], stroke);
    tt(response, key)
}

fn field_label(ui: &mut Ui, label: &str) {
    let width = stable_label_width(ui);
    let response = right_label(ui, width, label);
    if let Some(hover) = i18n::hover_for(None, label) {
        response.on_hover_text(hover);
    }
}

/// Form label with its right edge flush against the input that follows.
///
/// Reserves a fixed-width box (so every input in a column starts at the same
/// x) and paints the text right-aligned inside it. A `Label` widget cannot do
/// this: `Ui::add_sized` centers its content, and a shrink-to-text galley in
/// a `top_down(Align::RIGHT)` box still lands on the left.
fn right_label(ui: &mut Ui, width: f32, label: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 22.0), Sense::hover());
    ui.painter().text(
        rect.right_center(),
        Align2::RIGHT_CENTER,
        label,
        FontId::proportional(12.0),
        TEXT_MUTED,
    );
    response
}

fn input_width(ui: &Ui) -> f32 {
    (ui.available_width() - 2.0).clamp(64.0, 360.0)
}

fn segmented_option_label(value: &str) -> String {
    value
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fixed form label width. Proportional label widths drift apart when the
/// window is resized or the UI is zoomed because each column measures a
/// different available width; one constant keeps every input in a column
/// starting at the same x. Very narrow panels still get a smaller box so
/// the input keeps some room.
const LABEL_WIDTH: f32 = 140.0;

fn stable_label_width(ui: &Ui) -> f32 {
    LABEL_WIDTH.min(ui.available_width() * 0.55).max(72.0)
}

/// Expand a leading `~/` against `$HOME` for filesystem access.
fn expand_theme_path(value: &str) -> PathBuf {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(value)
}

/// Directory containing the config file, with `~` expanded.
fn theme_config_dir(config_path: &str) -> PathBuf {
    let expanded = expand_theme_path(config_path.trim());
    expanded
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Collapse an absolute path under `$HOME` back to `~/...`, matching the
/// stored theme style.
fn collapse_home(path: &Path) -> String {
    if let Some(home) = env::var_os("HOME") {
        let home = PathBuf::from(home);
        if let Ok(rest) = path.strip_prefix(&home) {
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Resolve a stored theme value the same way the daemon does: `~/` against
/// `$HOME`, absolute as-is, relative against the config directory.
fn normalize_theme_path(value: &str, config_path: &str) -> PathBuf {
    let expanded = expand_theme_path(value.trim());
    if expanded.is_absolute() {
        expanded
    } else {
        theme_config_dir(config_path).join(expanded)
    }
}

fn theme_display_name(path: &str) -> &str {
    Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(path)
}

/// Theme files beside the config (`themes/*.toml` and `*.toml`, excluding
/// the config itself), collapsed to `~/...` and sorted. Only existing files
/// are ever offered, so the dropdown can never select an invalid path.
fn theme_candidates(config_path: &str) -> Vec<String> {
    let dir = theme_config_dir(config_path);
    let config_file = expand_theme_path(config_path.trim())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_string();
    let mut candidates = Vec::new();
    for scan in [dir.join("themes"), dir.clone()] {
        let Ok(entries) = fs::read_dir(&scan) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
                continue;
            }
            if scan == dir
                && path.file_name().and_then(|name| name.to_str()) == Some(config_file.as_str())
            {
                continue;
            }
            if !path.is_file() {
                continue;
            }
            let collapsed = collapse_home(&path);
            if !candidates.contains(&collapsed) {
                candidates.push(collapsed);
            }
        }
    }
    candidates.sort();
    candidates
}

/// Seed content for a new theme file: a `[style]` table with the currently
/// edited background/accent/text (or built-in defaults), which always
/// passes the daemon theme validator.
fn theme_seed(model: &Value) -> String {
    let pick =
        |key: &str, fallback: &str| model["style"][key].as_str().unwrap_or(fallback).to_string();
    format!(
        "# Created by niri-xmb Config Studio. Every key under [style] is\n\
         # optional; delete a line to fall back to the built-in default.\n\
         [style]\n\
         background = \"{}\"\n\
         accent = \"{}\"\n\
         text = \"{}\"\n",
        pick("background", "#f2141218"),
        pick("accent", "#ffd0bcff"),
        pick("text", "#ffe6e1e5"),
    )
}

/// Create `themes/custom-N.toml` beside the config with valid seeded
/// defaults and return its collapsed path. The file is validated before it
/// is handed back, so an invalid path is never installed as a placeholder.
fn create_theme_file(config_path: &str, model: &Value) -> Option<String> {
    let dir = theme_config_dir(config_path);
    let themes = dir.join("themes");
    fs::create_dir_all(&themes).ok()?;
    let expanded_config = expand_theme_path(config_path.trim());
    for index in 1..1000 {
        let candidate = themes.join(format!("custom-{index}.toml"));
        if candidate.exists() {
            continue;
        }
        fs::write(&candidate, theme_seed(model)).ok()?;
        let collapsed = collapse_home(&candidate);
        if Config::validate_theme_reference(&collapsed, &expanded_config).is_err() {
            fs::remove_file(&candidate).ok()?;
            return None;
        }
        return Some(collapsed);
    }
    None
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

/// Determine the button code and default action to auto-fill when adding a binding.
/// Prefers the currently selected controller hotspot/button; if already bound in
/// this table or unmapped, searches the template hotspots in selection order, then
/// falls back to unbound standard gamepad buttons. Avoids empty/invalid states.
fn next_binding_entry<'a>(
    template: Template,
    selected: &str,
    path: &str,
    actions: &[&'a str],
    used: &[String],
) -> Option<(&'static str, &'a str)> {
    let code = next_binding_code_for(template, selected, used)?;
    let action = default_action_for_code(path, actions, code);
    Some((code, action))
}

fn next_binding_code_for(
    template: Template,
    selected: &str,
    used: &[String],
) -> Option<&'static str> {
    let is_used = |code: &str| used.iter().any(|u| u == code);

    // 1. Prefer currently selected controller hotspot if it has an unbound button code.
    let hotspots = template.hotspots();
    if let Some(code) = hotspots
        .iter()
        .find(|h| h.control == selected)
        .and_then(|h| h.code)
        && !is_used(code)
    {
        return Some(code);
    }

    // 2. Preserve selection order: search hotspots starting after selected control.
    let selected_idx = hotspots
        .iter()
        .position(|h| h.control == selected)
        .unwrap_or(0);

    for offset in 1..=hotspots.len() {
        let idx = (selected_idx + offset) % hotspots.len();
        if let Some(code) = hotspots[idx].code
            && !is_used(code)
        {
            return Some(code);
        }
    }

    // 3. Fall back to remaining standard gamepad buttons in BUTTONS.
    for (code, _) in BUTTONS {
        if !is_used(code) {
            return Some(*code);
        }
    }

    None
}

fn default_action_for_code<'a>(path: &str, actions: &[&'a str], code: &str) -> &'a str {
    if actions.is_empty() {
        return "";
    }
    let preferred = if path.contains("buttons") {
        // XMB actions
        match code {
            "BTN_DPAD_UP" => "up",
            "BTN_DPAD_DOWN" => "down",
            "BTN_DPAD_LEFT" => "left",
            "BTN_DPAD_RIGHT" => "right",
            "BTN_SOUTH" | "BTN_START" | "BTN_TR" => "confirm",
            "BTN_EAST" | "BTN_MODE" => "close",
            "BTN_WEST" | "BTN_SELECT" | "BTN_TL" => "back",
            "BTN_NORTH" => "tab",
            _ => "confirm",
        }
    } else {
        // Navi base / layer actions
        match code {
            "BTN_SOUTH" => "pointer.left",
            "BTN_EAST" => "pointer.right",
            "BTN_WEST" => "pointer.middle",
            "BTN_NORTH" => "xmb.open",
            "BTN_START" => "niri.toggle-overview",
            "BTN_DPAD_UP" => "pointer.scroll-up",
            "BTN_DPAD_DOWN" => "pointer.scroll-down",
            _ => "pointer.left",
        }
    };
    if actions.contains(&preferred) {
        preferred
    } else {
        actions[0]
    }
}

fn is_left_stick(control: &str) -> bool {
    matches!(control, "L STICK" | "L3")
}

fn is_right_stick(control: &str) -> bool {
    matches!(control, "R STICK" | "R3")
}

fn stick_control_for_template(template: Template, stick: &str) -> &'static str {
    match (stick, template) {
        ("left", Template::Ds4) => "L3",
        ("left", Template::Xbox) => "L STICK",
        ("right", Template::Ds4) => "R3",
        ("right", Template::Xbox) => "R STICK",
        _ => "L STICK",
    }
}

fn control_name_for_code_and_template(template: Template, code: &str) -> &'static str {
    if let Some(control) = template
        .hotspots()
        .iter()
        .find(|h| h.code == Some(code))
        .map(|h| h.control)
    {
        return control;
    }
    match code {
        "BTN_TRIGGER_HAPPY1" => "Paddle 1",
        "BTN_TRIGGER_HAPPY2" => "Paddle 2",
        "BTN_TRIGGER_HAPPY3" => "Paddle 3",
        "BTN_TRIGGER_HAPPY4" => "Paddle 4",
        "BTN_DPAD_UP" => "D-pad up",
        "BTN_DPAD_DOWN" => "D-pad down",
        "BTN_DPAD_LEFT" => "D-pad left",
        "BTN_DPAD_RIGHT" => "D-pad right",
        "BTN_SOUTH" => {
            if template == Template::Ds4 {
                "Cross"
            } else {
                "A"
            }
        }
        "BTN_EAST" => {
            if template == Template::Ds4 {
                "Circle"
            } else {
                "B"
            }
        }
        "BTN_WEST" => {
            if template == Template::Ds4 {
                "Square"
            } else {
                "X"
            }
        }
        "BTN_NORTH" => {
            if template == Template::Ds4 {
                "Triangle"
            } else {
                "Y"
            }
        }
        "BTN_TL" => {
            if template == Template::Ds4 {
                "L1"
            } else {
                "LB"
            }
        }
        "BTN_TR" => {
            if template == Template::Ds4 {
                "R1"
            } else {
                "RB"
            }
        }
        "BTN_TL2" => {
            if template == Template::Ds4 {
                "L2"
            } else {
                "LT"
            }
        }
        "BTN_TR2" => {
            if template == Template::Ds4 {
                "R2"
            } else {
                "RT"
            }
        }
        "BTN_THUMBL" => {
            if template == Template::Ds4 {
                "L3"
            } else {
                "L STICK"
            }
        }
        "BTN_THUMBR" => {
            if template == Template::Ds4 {
                "R3"
            } else {
                "R STICK"
            }
        }
        _ => "Button",
    }
}

fn paint_binding_link(ui: &Ui, from: Rect, to: Rect) {
    let clip = ui.clip_rect();
    if !clip.intersects(from) || !clip.intersects(to) {
        return;
    }
    let painter = ui.painter();
    let color = Color32::from_rgba_unmultiplied(116, 162, 255, 180);

    if to.left() >= from.center().x {
        // Horizontal connection (side-by-side wide layout)
        let start = Pos2::new(from.right() + 4.0, from.center().y);
        let end = Pos2::new(to.left() - 6.0, to.center().y);
        let reach = ((end.x - start.x) * 0.45).clamp(20.0, 90.0);
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
    } else if to.top() > from.bottom() {
        // Vertical connection (stacked narrow layout)
        let start = Pos2::new(from.center().x, from.bottom() + 4.0);
        let end = Pos2::new(to.center().x, to.top() - 6.0);
        let reach = ((end.y - start.y) * 0.45).clamp(15.0, 60.0);
        painter.add(egui::epaint::CubicBezierShape::from_points_stroke(
            [
                start,
                Pos2::new(start.x, start.y + reach),
                Pos2::new(end.x, end.y - reach),
                end,
            ],
            false,
            Color32::TRANSPARENT,
            Stroke::new(1.5_f32, color),
        ));
        painter.circle_filled(start, 2.5, color);
        painter.circle_filled(end, 3.0, color);
    }
}

fn draw_cursor_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let size = rect.width().min(rect.height()).clamp(14.0, 20.0);
    let origin = rect.center() - Vec2::new(size * 0.45, size * 0.50);
    let pts = [
        origin + Vec2::new(0.0, 0.0),
        origin + Vec2::new(size * 0.82, size * 0.60),
        origin + Vec2::new(size * 0.48, size * 0.60),
        origin + Vec2::new(size * 0.70, size * 1.02),
        origin + Vec2::new(size * 0.52, size * 1.08),
        origin + Vec2::new(size * 0.32, size * 0.68),
        origin + Vec2::new(0.0, size * 0.85),
    ];
    painter.add(egui::Shape::convex_polygon(
        pts.to_vec(),
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 45),
        Stroke::new(1.4_f32, color),
    ));
}

fn draw_scroll_wheel_icon(painter: &egui::Painter, rect: Rect, color: Color32) {
    let center = rect.center();
    let w = 12.0_f32;
    let h = 18.0_f32;
    let body_rect = Rect::from_center_size(center, Vec2::new(w, h));
    painter.rect_filled(
        body_rect,
        5.0,
        Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 30),
    );
    painter.rect_stroke(
        body_rect,
        5.0,
        Stroke::new(1.3_f32, color),
        StrokeKind::Inside,
    );
    // Upper center wheel capsule
    let wheel_rect =
        Rect::from_center_size(Pos2::new(center.x, center.y - 3.2), Vec2::new(3.0, 6.0));
    painter.rect_filled(wheel_rect, 1.5, color);
    // Subtle tick line below wheel
    painter.line_segment(
        [
            Pos2::new(center.x, center.y + 2.5),
            Pos2::new(center.x, center.y + 5.5),
        ],
        Stroke::new(
            1.0_f32,
            Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 120),
        ),
    );
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

/// Clickable disclosure heading with the same vector chevron the telemetry
/// header uses. Returns true exactly once per click so the caller can flip its
/// own state field.
fn collapsible_heading(ui: &mut Ui, text: &str, expanded: bool) -> bool {
    let mut toggle = false;
    ui.horizontal(|ui| {
        let (chevron_rect, chevron) = ui.allocate_exact_size(Vec2::new(24.0, 22.0), Sense::click());
        let center = chevron_rect.center();
        let (p, a, b) = if expanded {
            (
                center,
                Pos2::new(center.x - 5.0, center.y - 2.0),
                Pos2::new(center.x + 5.0, center.y - 2.0),
            )
        } else {
            (
                Pos2::new(center.x + 1.5, center.y),
                Pos2::new(center.x - 3.5, center.y - 5.0),
                Pos2::new(center.x - 3.5, center.y + 5.0),
            )
        };
        let stroke = Stroke::new(
            1.8_f32,
            if chevron.hovered() {
                ACCENT
            } else {
                TEXT_MUTED
            },
        );
        ui.painter().line_segment([a, p], stroke);
        ui.painter().line_segment([p, b], stroke);
        let label =
            ui.add(egui::Label::new(RichText::new(text).size(12.0).strong()).sense(Sense::click()));
        let label = if let Some(hover) = i18n::hover_for(None, text) {
            label.on_hover_text(hover)
        } else {
            label
        };
        toggle = chevron.clicked() || label.clicked();
    });
    toggle
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

#[allow(dead_code)]
fn multiplier_pair_row(ui: &mut Ui, label: &str, target: f32, actual: f32, enabled: bool) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{label} speed"))
                .size(11.0)
                .color(TEXT_MUTED),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(
                RichText::new(format!("actual {actual:.2}× · target {target:.2}×"))
                    .monospace()
                    .size(12.0)
                    .color(if enabled { TEXT } else { TEXT_DIM }),
            );
            if !enabled {
                ui.label(RichText::new("off").size(10.0).color(WARN));
            }
        });
    });
}

#[allow(dead_code)]
fn smooth_display_multiplier(previous: f32, target: f32, smoothing_ms: f32, dt: f32) -> f32 {
    if smoothing_ms <= 0.0 || dt <= 0.0 {
        target
    } else {
        let tau = smoothing_ms / 1000.0;
        let alpha = 1.0 - (-dt / tau).exp();
        previous + (target - previous) * alpha
    }
}

#[allow(dead_code)]
fn live_pedal_meter(
    ui: &mut Ui,
    label: &str,
    raw: f32,
    deadzone: f32,
    curve: f32,
    points: &[CurvePoint],
    color: Color32,
) {
    let shaped = trigger_shape(raw, deadzone, curve, points);
    ui.vertical(|ui| {
        ui.label(RichText::new(label).size(11.0).color(TEXT));
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 18.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, FIELD_BG);
        painter.rect_stroke(rect, 4.0, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
        let raw_rect = Rect::from_min_max(
            rect.min,
            Pos2::new(rect.min.x + rect.width() * raw, rect.max.y),
        );
        painter.rect_filled(raw_rect, 4.0, color.gamma_multiply(0.25));
        let shaped_rect = Rect::from_min_max(
            rect.min,
            Pos2::new(rect.min.x + rect.width() * shaped, rect.max.y),
        );
        painter.rect_filled(shaped_rect, 4.0, color);
        let dz_x = rect.min.x + rect.width() * deadzone;
        painter.line_segment(
            [
                Pos2::new(dz_x, rect.min.y - 3.0),
                Pos2::new(dz_x, rect.max.y + 3.0),
            ],
            Stroke::new(1.0_f32, TEXT_MUTED),
        );
        ui.label(
            RichText::new(format!(
                "raw {:.0}%  →  shaped {:.0}%  · deadzone {:.0}%",
                raw * 100.0,
                shaped * 100.0,
                deadzone * 100.0
            ))
            .monospace()
            .size(9.0)
            .color(TEXT_DIM),
        );
    });
}

#[allow(dead_code, clippy::too_many_arguments)]
fn live_stick_summary(
    ui: &mut Ui,
    label: &str,
    coordinate: (f32, f32),
    deadzone: f32,
    curve: f32,
    speed: f32,
    units: &str,
    multiplier: f32,
    scroll_axis: bool,
    live: bool,
) {
    let (x, y) = coordinate;
    let magnitude = (x * x + y * y).sqrt().min(1.0);
    let (effective_x, effective_y) = if !live {
        (0.0, 0.0)
    } else if scroll_axis {
        let value = y.abs();
        (
            x,
            if value <= deadzone {
                0.0
            } else {
                y.signum() * ((value - deadzone) / (1.0 - deadzone).max(0.001))
            },
        )
    } else if magnitude <= deadzone {
        (0.0, 0.0)
    } else {
        let scaled = ((magnitude - deadzone) / (1.0 - deadzone).max(0.001)).min(1.0);
        (x / magnitude * scaled, y / magnitude * scaled)
    };
    let shaped_x = effective_x.signum() * effective_x.abs().powf(curve.max(0.05));
    let shaped_y = effective_y.signum() * effective_y.abs().powf(curve.max(0.05));
    let base_speed = if scroll_axis {
        shaped_y.abs() * speed
    } else {
        (shaped_x * shaped_x + shaped_y * shaped_y).sqrt() * speed
    };
    let effective_speed = base_speed * multiplier;
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(86.0), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 5.0, FIELD_BG);
        painter.rect_stroke(rect, 5.0, Stroke::new(1.0_f32, BORDER), StrokeKind::Inside);
        let center = rect.center();
        let radius = rect.width().min(rect.height()) * 0.36;
        painter.circle_stroke(center, radius, Stroke::new(1.0_f32, TEXT_DIM));
        painter.circle_stroke(center, radius * deadzone, Stroke::new(1.0_f32, BORDER));
        painter.line_segment(
            [
                Pos2::new(center.x - radius, center.y),
                Pos2::new(center.x + radius, center.y),
            ],
            Stroke::new(1.0_f32, BORDER),
        );
        painter.line_segment(
            [
                Pos2::new(center.x, center.y - radius),
                Pos2::new(center.x, center.y + radius),
            ],
            Stroke::new(1.0_f32, BORDER),
        );
        let point = Pos2::new(
            center.x + effective_x * radius,
            center.y + effective_y * radius,
        );
        painter.line_segment([center, point], Stroke::new(1.5_f32, ACCENT));
        painter.circle_filled(point, 4.0, if live { ACCENT } else { TEXT_DIM });
        ui.vertical(|ui| {
            ui.label(RichText::new(label).size(11.0).color(TEXT));
            ui.label(
                RichText::new(if live {
                    format!("x {x:+.2}  y {y:+.2}")
                } else {
                    "no live signal".to_string()
                })
                .monospace()
                .size(9.0)
                .color(TEXT_MUTED),
            );
            ui.label(
                RichText::new(format!(
                    "magnitude {:.0}% · base {:.1} · output {:.1} {units}",
                    magnitude * 100.0,
                    base_speed,
                    effective_speed,
                ))
                .monospace()
                .size(9.0)
                .color(TEXT_DIM),
            );
        });
    });
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

fn control_hint(template: Template, control: &str) -> Option<&'static str> {
    match control {
        "L STICK" | "R STICK" | "L3" | "R3" => {
            Some("Pointer and scroll settings are under Pointer.")
        }
        "LT" | "RT" | "L2" | "R2" => {
            Some("Curves and pedals are under Trigger response and Trigger settings.")
        }
        "D-PAD" | "D-pad up" | "D-pad down" | "D-pad left" | "D-pad right" | "D-pad Up"
        | "D-pad Down" | "D-pad Left" | "D-pad Right" => {
            Some("D-pad actions resolve through the active layer or XMB.")
        }
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

#[allow(dead_code)]
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

/// Serialise a picked colour back to the daemon's `#AARRGGBB` hex syntax
/// (alpha first), matching `StyleColor`'s display format. Un-premultiplies
/// first: `Color32` stores premultiplied channels, so writing the raw values
/// would darken every translucent colour on each picker edit.
fn color_to_hex(color: Color32) -> String {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    format!("#{a:02X}{r:02X}{g:02X}{b:02X}")
}

fn main() -> Result<()> {
    if env::args().any(|arg| arg == "--print-strings") {
        print!("{}", i18n::en_template_json());
        return Ok(());
    }
    let (config, niri_xmb) = parse_args()?;
    // Single instance with takeover: a new launch terminates the old studio
    // and becomes active, so duplicate windows and orphaned subscribe streams
    // cannot accumulate. The guard holds the flock until process exit.
    let _instance = single_instance::acquire_single_instance()?;
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
    fn popup_tabs_keep_the_popup_sections_in_order() {
        assert_eq!(PopupTab::ALL.len(), 4);
        assert_eq!(
            PopupTab::ALL.map(PopupTab::label),
            [
                "Panel & Placements",
                "Behavior, Peek & Animation",
                "Theme & Colors",
                "Bars",
            ]
        );
        assert_eq!(segmented_option_label("follow_active"), "Follow Active");
        assert_eq!(segmented_option_label("center"), "Center");
    }

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
    fn telemetry_state_reports_stream_health_not_frame_age() {
        // An idle desktop pushes no gamepad frames: a connected controller
        // with old frames is idle, never stale.
        assert_eq!(
            telemetry_state(true, true, 1, Some(100), true),
            TelemetryState::Live
        );
        assert_eq!(
            telemetry_state(true, true, 1, Some(5_000), true),
            TelemetryState::Idle
        );
        assert_eq!(
            telemetry_state(true, true, 1, None, false),
            TelemetryState::Idle
        );
        // Buttons without axes are still input channels when recent.
        assert_eq!(
            telemetry_state(true, true, 1, Some(10), true),
            TelemetryState::Live
        );
        // No controller is its own state while the stream is alive.
        assert_eq!(
            telemetry_state(true, true, 0, None, false),
            TelemetryState::NoController
        );
        // A dead stream reports reconnecting when a status was seeded,
        // offline when nothing was ever received.
        assert_eq!(
            telemetry_state(true, false, 1, Some(10), true),
            TelemetryState::Reconnecting
        );
        assert_eq!(
            telemetry_state(false, false, 0, None, false),
            TelemetryState::Offline
        );
    }

    #[test]
    fn length_values_parse_pixels_and_percent() {
        assert_eq!(
            parse_length_value(&json!(320)),
            (LengthUnit::Pixels, "320".to_string(), Some(320.0))
        );
        assert_eq!(
            parse_length_value(&json!("25%")),
            (LengthUnit::Percent, "25".to_string(), Some(25.0))
        );
    }

    #[test]
    fn length_values_format_without_changing_static_pixel_fields() {
        assert_eq!(format_length_value(LengthUnit::Pixels, "320"), json!(320.0));
        assert_eq!(format_length_value(LengthUnit::Percent, "25"), json!("25%"));
    }

    #[test]
    fn length_unit_toggle_round_trips_with_live_extent() {
        let bounds = config_length_bounds("popup.width").unwrap();
        let (unit, percent) = toggle_length_value(LengthUnit::Pixels, 960.0, Some(1920.0), bounds);
        assert_eq!(unit, LengthUnit::Percent);
        assert!((percent - 50.0).abs() < f64::EPSILON);
        let (unit, pixels) = toggle_length_value(unit, percent, Some(1920.0), bounds);
        assert_eq!(unit, LengthUnit::Pixels);
        assert!((pixels - 960.0).abs() < f64::EPSILON);
    }

    #[test]
    fn length_unit_toggle_without_extent_keeps_the_magnitude() {
        let bounds = config_length_bounds("popup.width").unwrap();
        let (unit, value) = toggle_length_value(LengthUnit::Pixels, 320.0, None, bounds);
        assert_eq!(unit, LengthUnit::Percent);
        assert_eq!(value, 320.0);
    }

    #[test]
    fn theme_seed_passes_daemon_validation() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/niri-xmb-studio-theme-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("themes")).unwrap();
        let config_path = dir.join("config.toml");
        fs::write(&config_path, "[popup]\n").unwrap();
        for model in [
            json!({}),
            json!({"style": {"background": "#f2141218", "accent": "#ffd0bcff"}}),
        ] {
            let seed_path = dir.join("themes").join("seed.toml");
            fs::write(&seed_path, theme_seed(&model)).unwrap();
            Config::validate_theme_reference("themes/seed.toml", &config_path)
                .expect("seeded theme must validate");
            fs::remove_file(&seed_path).ok();
        }
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn theme_discovery_lists_files_and_creation_selects_valid_file() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/niri-xmb-studio-theme-new-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("themes")).unwrap();
        let config_path = dir.join("config.toml");
        fs::write(&config_path, "[popup]\n").unwrap();
        fs::write(dir.join("themes").join("a.toml"), "[style]\n").unwrap();
        fs::write(dir.join("themes").join("b.toml"), "[style]\n").unwrap();
        let config_str = config_path.display().to_string();
        let candidates = theme_candidates(&config_str);
        assert_eq!(candidates.len(), 2, "{candidates:?}");
        assert!(candidates.iter().all(|path| path.ends_with(".toml")));
        // The config file itself is never offered, even though it is TOML.
        assert!(!candidates.iter().any(|path| path.ends_with("config.toml")));

        let created = create_theme_file(&config_str, &json!({})).expect("create theme");
        assert!(created.ends_with("custom-1.toml"), "{created}");
        assert!(
            normalize_theme_path(&created, &config_str).is_file(),
            "created file must exist"
        );
        Config::validate_theme_reference(&created, &expand_theme_path(&config_str))
            .expect("created theme must validate");
        let again = theme_candidates(&config_str);
        assert_eq!(again.len(), 3, "{again:?}");
        let second = create_theme_file(&config_str, &json!({})).expect("create again");
        assert_ne!(created, second);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn field_rows_stay_aligned_across_window_widths() {
        // Zoom/resize drift: label right edges must line up per column at
        // every width, so inputs start at the same x no matter the scale.
        let left = COLOR_FIELDS.len().div_ceil(2);
        let wanted: Vec<String> = COLOR_FIELDS
            .iter()
            .map(|(path, label)| i18n::label_for_path(path, label).to_string())
            .collect();
        for width in [560.0_f32, 900.0, 1440.0, 2200.0] {
            let ctx = egui::Context::default();
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::Vec2::new(width, 900.0),
                )),
                ..Default::default()
            };
            let output = ctx.run(raw, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.columns(2, |cols| {
                        for (index, (path, label)) in COLOR_FIELDS.iter().copied().enumerate() {
                            let column = usize::from(index >= left);
                            field_row_path(&mut cols[column], Some(path), label, |ui| {
                                ui.label("x");
                                true
                            });
                        }
                    });
                });
            });
            let mut edges: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
            for shape in &output.shapes {
                if let egui::epaint::Shape::Text(text) = &shape.shape
                    && let Some(index) = wanted
                        .iter()
                        .position(|want| want.as_str() == text.galley.text())
                {
                    let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                    edges[usize::from(index >= left)].push(rect.right());
                }
            }
            for (column, group) in edges.iter().enumerate() {
                let spread = group.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                    - group.iter().cloned().fold(f32::INFINITY, f32::min);
                assert!(
                    spread <= 1.5,
                    "width {width}: column {column} right edges differ by {spread:.1}px"
                );
            }
        }
    }

    #[test]
    fn live_gamepad_view_is_empty_without_the_live_field() {
        let view = GamepadLiveView::from_status(&json!({"gamepad": {"devices": 0}}));
        assert!(view.buttons.is_empty());
        assert!(view.axes.is_empty());
    }

    #[test]
    fn live_gamepad_view_exposes_both_stick_coordinates_and_device_state() {
        let status = json!({
            "gamepad": {
                "connected": [{"name": "Test Pad", "path": "/dev/input/event9", "preset": "ds4", "vendor": 0x054c, "product": 0x05c4}],
                "live": [{"axes": {
                    "ABS_X": 0.25, "ABS_Y": -0.5,
                    "ABS_RX": -0.75, "ABS_RY": 0.4
                }}]
            }
        });
        let view = GamepadLiveView::from_status(&status);
        assert_eq!(view.connected, 1);
        assert_eq!(view.device_name, "Test Pad");
        assert_eq!(view.device_path, "/dev/input/event9");
        assert_eq!(view.preset, "ds4");
        assert_eq!((view.vendor, view.product), (0x054c, 0x05c4));
        assert_eq!(view.stick(true), (0.25, -0.5));
        assert_eq!(view.stick(false), (-0.75, 0.4));
    }

    #[test]
    fn telemetry_display_axes_shows_neutral_xbox_controls_when_idle() {
        // Idle-but-connected Xbox/evdev controller: no live axes yet, but the
        // panel must still show neutral sticks, triggers, and hat.
        let status = json!({
            "gamepad": {
                "connected": [{
                    "name": "Xbox Wireless Controller",
                    "path": "/dev/input/by-id/usb-Microsoft_Xbox-event-joystick",
                    "preset": "xbox",
                    "vendor": 0x045e,
                    "product": 0x02fd
                }],
                "live": [{
                    "path": "/dev/input/by-id/usb-Microsoft_Xbox-event-joystick",
                    "buttons": [],
                    "axes": {}
                }]
            }
        });
        let view = GamepadLiveView::from_status(&status);
        assert_eq!(view.connected, 1);
        assert_eq!(view.stick(true), (0.0, 0.0));
        assert_eq!(view.stick(false), (0.0, 0.0));
        assert_eq!(view.trigger("ABS_Z", "BTN_TL2"), 0.0);
        assert_eq!(view.trigger("ABS_RZ", "BTN_TR2"), 0.0);
        let rows = telemetry_display_axes(&view);
        for axis in [
            "ABS_X",
            "ABS_Y",
            "ABS_RX",
            "ABS_RY",
            "ABS_Z",
            "ABS_RZ",
            "ABS_HAT0X",
            "ABS_HAT0Y",
        ] {
            let value = rows
                .iter()
                .find(|(name, _)| name == axis)
                .map(|(_, value)| *value)
                .expect("expected axis row stays visible at neutral");
            assert_eq!(value, 0.0, "{axis} neutral must read 0.0");
        }
    }

    #[test]
    fn telemetry_display_axes_shows_live_xbox_sticks_triggers_and_buttons() {
        // Non-neutral Xbox/evdev frame: both sticks, both triggers, one button.
        let status = json!({
            "gamepad": {
                "connected": [{
                    "name": "Xbox Wireless Controller",
                    "path": "/dev/input/by-id/usb-Microsoft_Xbox-event-joystick",
                    "preset": "xbox",
                    "vendor": 0x045e,
                    "product": 0x02fd
                }],
                "live": [{
                    "path": "/dev/input/by-id/usb-Microsoft_Xbox-event-joystick",
                    "buttons": ["BTN_SOUTH"],
                    "axes": {
                        "ABS_X": 0.5, "ABS_Y": -0.25,
                        "ABS_RX": -0.75, "ABS_RY": 0.4,
                        "ABS_Z": 0.8, "ABS_RZ": 0.0,
                        "ABS_HAT0X": 1.0, "ABS_HAT0Y": 0.0
                    }
                }]
            }
        });
        let view = GamepadLiveView::from_status(&status);
        assert!(view.button("BTN_SOUTH"));
        assert_eq!(view.stick(true), (0.5, -0.25));
        assert_eq!(view.stick(false), (-0.75, 0.4));
        assert_eq!(view.trigger("ABS_Z", "BTN_TL2"), 0.8);
        assert_eq!(view.trigger("ABS_RZ", "BTN_TR2"), 0.0);
        assert!(view.dpad("right"));
        let rows: std::collections::HashMap<_, _> =
            telemetry_display_axes(&view).into_iter().collect();
        assert_eq!(rows["ABS_X"], 0.5);
        assert_eq!(rows["ABS_Y"], -0.25);
        assert_eq!(rows["ABS_RX"], -0.75);
        assert_eq!(rows["ABS_RY"], 0.4);
        assert_eq!(rows["ABS_Z"], 0.8);
        assert_eq!(rows["ABS_RZ"], 0.0);
    }

    #[test]
    fn display_multiplier_smoothing_is_instant_or_exponential() {
        assert_eq!(smooth_display_multiplier(1.0, 2.5, 0.0, 0.01), 2.5);
        let first = smooth_display_multiplier(1.0, 2.5, 100.0, 0.1);
        assert!((first - 1.94818).abs() < 1e-4);
        let settled = smooth_display_multiplier(first, 2.5, 100.0, 1.0);
        assert!((settled - 2.49993).abs() < 1e-4);
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
    fn color_hex_round_trips_through_picker_format() {
        // Opaque `#RRGGBB` parses to full alpha and writes back explicitly.
        assert_eq!(color_to_hex(style_color("#E9E1E0")), "#FFE9E1E0");
        // Translucent `#AARRGGBB` survives a picker edit within one step per
        // channel (`Color32` stores premultiplied values) and is stable:
        // a second edit changes nothing further.
        for hex in ["#B8221D1D", "#FFDEBFC2", "#36000000"] {
            let once = color_to_hex(style_color(hex));
            let twice = color_to_hex(style_color(&once));
            assert_eq!(once, twice, "picker edit should be stable for {hex}");
            for (got, expected) in
                once.chars()
                    .skip(1)
                    .zip(hex.chars().skip(1))
                    .map(|(got, expected)| {
                        (
                            u8::from_str_radix(&got.to_string(), 16).unwrap() as i16,
                            u8::from_str_radix(&expected.to_string(), 16).unwrap() as i16,
                        )
                    })
            {
                assert!(
                    (got - expected).abs() <= 1,
                    "picker edit of {hex} drifted to {once}"
                );
            }
        }
        // Invalid input never panics; it previews as transparent.
        assert_eq!(style_color("not a colour"), Color32::TRANSPARENT);
    }

    #[test]
    fn field_row_labels_share_a_right_edge_per_column() {
        // Regression test for the Colors card: labels used to be centered in
        // their fixed-width box (`Ui::add_sized` centers its content), leaving
        // both edges ragged. They must be right-aligned so every input starts
        // at the same x. Runs headless on the embedded font; no display needed.
        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::Vec2::new(1440.0, 900.0),
            )),
            ..Default::default()
        };
        // Same two-column split as `style_fields`.
        let left = COLOR_FIELDS.len().div_ceil(2);
        let output = ctx.run(raw, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.columns(2, |cols| {
                    for (index, (path, label)) in COLOR_FIELDS.iter().copied().enumerate() {
                        let column = usize::from(index >= left);
                        field_row_path(&mut cols[column], Some(path), label, |ui| {
                            ui.label("x");
                            true
                        });
                    }
                });
            });
        });
        let wanted: Vec<String> = COLOR_FIELDS
            .iter()
            .map(|(path, label)| i18n::label_for_path(path, label).to_string())
            .collect();
        let mut edges: [Vec<(f32, f32)>; 2] = [Vec::new(), Vec::new()];
        for shape in &output.shapes {
            if let egui::epaint::Shape::Text(text) = &shape.shape
                && let Some(index) = wanted
                    .iter()
                    .position(|want| want.as_str() == text.galley.text())
            {
                let rect = egui::Rect::from_min_size(text.pos, text.galley.size());
                edges[usize::from(index >= left)].push((rect.left(), rect.right()));
            }
        }
        let spread = |xs: &[f32]| {
            xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max)
                - xs.iter().cloned().fold(f32::INFINITY, f32::min)
        };
        for (column, group) in edges.iter().enumerate() {
            let expected = if column == 0 {
                left
            } else {
                COLOR_FIELDS.len() - left
            };
            assert_eq!(
                group.len(),
                expected,
                "column {column} should render {expected} labels"
            );
            let rights: Vec<f32> = group.iter().map(|&(_, right)| right).collect();
            let lefts: Vec<f32> = group.iter().map(|&(left, _)| left).collect();
            assert!(
                spread(&rights) <= 1.5,
                "column {column} label right edges differ by {:.1}px: {rights:?}",
                spread(&rights)
            );
            assert!(
                spread(&lefts) > 20.0,
                "column {column} labels look left-aligned or degenerate: {lefts:?}"
            );
        }
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
        // Wide layouts must not stretch one key across the panel.
        let wide = keyboard_cell(Vec2::new(1800.0, 400.0));
        assert_eq!(wide, KEYBOARD_MAX_KEY);

        // Short layouts are height-bound, so keys shrink with them.
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
        // Width-bound layouts still respect the column budget.
        let narrow = keyboard_cell(Vec2::new(240.0, 400.0));
        assert!(15.0 * narrow + 10.0 <= 240.0);
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
            ("D-pad up", "BTN_DPAD_UP"),
            ("D-pad down", "BTN_DPAD_DOWN"),
            ("D-pad left", "BTN_DPAD_LEFT"),
            ("D-pad right", "BTN_DPAD_RIGHT"),
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
    fn xbox_hotspots_map_linux_button_codes_and_separate_dpad() {
        let hotspots = Template::Xbox.hotspots();
        for (control, code) in [
            ("A", "BTN_SOUTH"),
            ("B", "BTN_EAST"),
            ("X", "BTN_WEST"),
            ("Y", "BTN_NORTH"),
            ("LB", "BTN_TL"),
            ("RB", "BTN_TR"),
            ("SELECT", "BTN_SELECT"),
            ("START", "BTN_START"),
            ("GUIDE", "BTN_MODE"),
            ("D-pad up", "BTN_DPAD_UP"),
            ("D-pad down", "BTN_DPAD_DOWN"),
            ("D-pad left", "BTN_DPAD_LEFT"),
            ("D-pad right", "BTN_DPAD_RIGHT"),
        ] {
            let hotspot = hotspots
                .iter()
                .find(|hotspot| hotspot.control == control)
                .unwrap_or_else(|| panic!("missing {control}"));
            assert_eq!(hotspot.code, Some(code), "{control}");
        }
    }

    #[test]
    fn template_switch_maps_equivalent_controls() {
        assert_eq!(Template::Xbox.default_control(), "A");
        assert_eq!(Template::Ds4.default_control(), "Cross");
        assert_eq!(Template::Xbox.counterpart("L2"), Some("LT"));
        assert_eq!(Template::Xbox.counterpart("L3"), Some("L STICK"));
        assert_eq!(Template::Ds4.counterpart("A"), Some("Cross"));
        assert_eq!(Template::Ds4.counterpart("L STICK"), Some("L3"));
        assert_eq!(Template::Ds4.counterpart("D-pad up"), Some("D-pad up"));
        assert_eq!(Template::Xbox.counterpart("D-pad down"), Some("D-pad down"));
        assert_eq!(Template::Ds4.counterpart("D-pad left"), Some("D-pad left"));
        assert_eq!(
            Template::Xbox.counterpart("D-pad right"),
            Some("D-pad right")
        );
    }

    #[test]
    fn vector_assets_and_dpad_hotspot_coverage() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let xbox_svg = fs::read_to_string(manifest_dir.join("../../assets/controller-xbox.svg"))
            .expect("controller-xbox.svg missing");
        let ds4_svg = fs::read_to_string(manifest_dir.join("../../assets/controller-ds4.svg"))
            .expect("controller-ds4.svg missing");

        for id in ["dpad-up", "dpad-down", "dpad-left", "dpad-right"] {
            assert!(xbox_svg.contains(id), "xbox svg missing {id}");
            assert!(ds4_svg.contains(id), "ds4 svg missing {id}");
        }

        assert!(
            !xbox_svg.contains(">LS<"),
            "xbox svg should not have clashing LS text"
        );
        assert!(
            !xbox_svg.contains(">RS<"),
            "xbox svg should not have clashing RS text"
        );
        assert!(
            !ds4_svg.contains(">L3<"),
            "ds4 svg should not have clashing L3 text"
        );
        assert!(
            !ds4_svg.contains(">R3<"),
            "ds4 svg should not have clashing R3 text"
        );

        let xbox_hs = Template::Xbox.hotspots();
        let ds4_hs = Template::Ds4.hotspots();
        assert!(xbox_hs.iter().any(|h| h.control == "L STICK" && h.label));
        assert!(xbox_hs.iter().any(|h| h.control == "R STICK" && h.label));
        assert!(ds4_hs.iter().any(|h| h.control == "L3" && h.label));
        assert!(ds4_hs.iter().any(|h| h.control == "R3" && h.label));

        for template in [Template::Xbox, Template::Ds4] {
            let hotspots = template.hotspots();
            let dpad_directions = ["D-pad up", "D-pad down", "D-pad left", "D-pad right"];
            for dir in dpad_directions {
                let hs = hotspots
                    .iter()
                    .find(|h| h.control == dir)
                    .unwrap_or_else(|| panic!("{:?} missing {}", template.label(), dir));
                assert!(
                    hs.code.is_some(),
                    "dpad hotspot {dir} must have a button code"
                );
                assert!(
                    hs.w > 0.02 && hs.h > 0.02,
                    "dpad hotspot {dir} must have non-zero size"
                );
            }
        }
    }

    #[test]
    fn dpad_polygon_matches_vector_artwork_geometry_and_palette() {
        let rect = Rect::from_min_size(Pos2::ZERO, Vec2::new(441.0, 403.0));

        let (stroke, fill, stroke_width) = DpadVectorArtwork::palette();
        assert_eq!(stroke, Color32::from_rgb(193, 215, 238));
        assert_eq!(fill, Color32::from_rgba_unmultiplied(193, 215, 238, 46));
        assert!((stroke_width - 2.0).abs() < 1e-4);
        assert_eq!(
            DpadVectorArtwork::global().arrow_color,
            Color32::from_rgb(193, 215, 238)
        );

        for shape in [
            HotspotShape::DpadUp,
            HotspotShape::DpadDown,
            HotspotShape::DpadLeft,
            HotspotShape::DpadRight,
        ] {
            let pts = StudioApp::dpad_polygon(Template::Xbox, rect, shape).unwrap();
            assert!(
                pts.len() > 20,
                "{shape:?} must have smooth curved boundary points"
            );
            let arrow = StudioApp::dpad_arrow(Template::Xbox, rect, shape).unwrap();
            assert_eq!(
                arrow.len(),
                3,
                "{shape:?} arrow must have 3 triangle vertices"
            );
        }

        // Inward tapering toward D-pad center (166, 238)
        let up = StudioApp::dpad_polygon(Template::Xbox, rect, HotspotShape::DpadUp).unwrap();
        let up_outer: Vec<_> = up.iter().filter(|p| p.y < 215.0).collect();
        let up_inner: Vec<_> = up.iter().filter(|p| p.y > 225.0).collect();
        let up_outer_width = up_outer
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            - up_outer.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let up_inner_width = up_inner
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            - up_inner.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        assert!(
            up_outer_width > up_inner_width,
            "DpadUp must taper inward toward center: outer={up_outer_width}, inner={up_inner_width}"
        );

        let down = StudioApp::dpad_polygon(Template::Xbox, rect, HotspotShape::DpadDown).unwrap();
        let down_outer: Vec<_> = down.iter().filter(|p| p.y > 260.0).collect();
        let down_inner: Vec<_> = down.iter().filter(|p| p.y < 250.0).collect();
        let down_outer_width = down_outer
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            - down_outer.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        let down_inner_width = down_inner
            .iter()
            .map(|p| p.x)
            .fold(f32::NEG_INFINITY, f32::max)
            - down_inner.iter().map(|p| p.x).fold(f32::INFINITY, f32::min);
        assert!(
            down_outer_width > down_inner_width,
            "DpadDown must taper inward toward center: outer={down_outer_width}, inner={down_inner_width}"
        );

        let left = StudioApp::dpad_polygon(Template::Xbox, rect, HotspotShape::DpadLeft).unwrap();
        let left_outer: Vec<_> = left.iter().filter(|p| p.x < 142.0).collect();
        let left_inner: Vec<_> = left.iter().filter(|p| p.x > 152.0).collect();
        let left_outer_height = left_outer
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            - left_outer.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let left_inner_height = left_inner
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            - left_inner.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        assert!(
            left_outer_height > left_inner_height,
            "DpadLeft must taper inward toward center: outer={left_outer_height}, inner={left_inner_height}"
        );

        let right = StudioApp::dpad_polygon(Template::Xbox, rect, HotspotShape::DpadRight).unwrap();
        let right_outer: Vec<_> = right.iter().filter(|p| p.x > 190.0).collect();
        let right_inner: Vec<_> = right.iter().filter(|p| p.x < 180.0).collect();
        let right_outer_height = right_outer
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            - right_outer
                .iter()
                .map(|p| p.y)
                .fold(f32::INFINITY, f32::min);
        let right_inner_height = right_inner
            .iter()
            .map(|p| p.y)
            .fold(f32::NEG_INFINITY, f32::max)
            - right_inner
                .iter()
                .map(|p| p.y)
                .fold(f32::INFINITY, f32::min);
        assert!(
            right_outer_height > right_inner_height,
            "DpadRight must taper inward toward center: outer={right_outer_height}, inner={right_inner_height}"
        );

        // Invariant: DS4 geometry is translated by (-53, -78) relative to Xbox geometry
        for shape in [
            HotspotShape::DpadUp,
            HotspotShape::DpadDown,
            HotspotShape::DpadLeft,
            HotspotShape::DpadRight,
        ] {
            let xbox_pts = StudioApp::dpad_polygon(Template::Xbox, rect, shape).unwrap();
            let ds4_pts = StudioApp::dpad_polygon(Template::Ds4, rect, shape).unwrap();
            assert_eq!(xbox_pts.len(), ds4_pts.len());
            for (p_xbox, p_ds4) in xbox_pts.iter().zip(ds4_pts.iter()) {
                assert!(
                    (p_xbox.x - 53.0 - p_ds4.x).abs() < 1e-3,
                    "DS4 x mismatch for {shape:?}: xbox={}, ds4={}",
                    p_xbox.x,
                    p_ds4.x
                );
                assert!(
                    (p_xbox.y - 78.0 - p_ds4.y).abs() < 1e-3,
                    "DS4 y mismatch for {shape:?}: xbox={}, ds4={}",
                    p_xbox.y,
                    p_ds4.y
                );
            }
        }
    }

    #[test]
    fn dpad_live_view_highlights_individual_directions() {
        let mut live = GamepadLiveView::default();
        let up_hs = Template::Xbox
            .hotspots()
            .iter()
            .find(|h| h.control == "D-pad up")
            .unwrap();
        let down_hs = Template::Xbox
            .hotspots()
            .iter()
            .find(|h| h.control == "D-pad down")
            .unwrap();

        assert!(!up_hs.is_pressed(&live));
        assert!(!down_hs.is_pressed(&live));

        // Button press
        live.buttons.insert("BTN_DPAD_UP".to_string());
        assert!(up_hs.is_pressed(&live));
        assert!(!down_hs.is_pressed(&live));

        // Hat axis press
        live.buttons.clear();
        live.axes.insert("ABS_HAT0Y".to_string(), 1.0); // down
        assert!(!up_hs.is_pressed(&live));
        assert!(down_hs.is_pressed(&live));
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

    #[test]
    fn add_binding_autofills_selected_button_and_valid_default() {
        // When 'A' is selected on Xbox, next binding should auto-fill BTN_SOUTH with default confirm for XMB
        let entry = next_binding_entry(Template::Xbox, "A", "gamepad.buttons", XMB_ACTIONS, &[]);
        assert_eq!(entry, Some(("BTN_SOUTH", "confirm")));

        // For Navi base, next binding should auto-fill BTN_SOUTH with default pointer.left
        let entry = next_binding_entry(Template::Xbox, "A", "gamepad.navi.bindings", ACTIONS, &[]);
        assert_eq!(entry, Some(("BTN_SOUTH", "pointer.left")));

        // For D-pad up, next binding should auto-fill BTN_DPAD_UP with default up
        let entry = next_binding_entry(
            Template::Xbox,
            "D-pad up",
            "gamepad.buttons",
            XMB_ACTIONS,
            &[],
        );
        assert_eq!(entry, Some(("BTN_DPAD_UP", "up")));
    }

    #[test]
    fn add_binding_preserves_selection_order_when_selected_already_bound() {
        let used = vec!["BTN_SOUTH".to_string()];
        // When 'A' (BTN_SOUTH) is already bound, next binding cycles hotspots in selection order.
        // In XBOX_HOTSPOTS order, after "A" comes "SELECT" (BTN_SELECT).
        let entry = next_binding_entry(Template::Xbox, "A", "gamepad.buttons", XMB_ACTIONS, &used);
        assert_eq!(entry, Some(("BTN_SELECT", "back")));

        let used_two = vec!["BTN_SOUTH".to_string(), "BTN_SELECT".to_string()];
        let entry = next_binding_entry(
            Template::Xbox,
            "A",
            "gamepad.buttons",
            XMB_ACTIONS,
            &used_two,
        );
        // After "SELECT" comes "START" (BTN_START).
        assert_eq!(entry, Some(("BTN_START", "confirm")));
    }

    #[test]
    fn add_binding_avoids_invalid_empty_state_when_all_bound() {
        let all_used: Vec<String> = BUTTONS.iter().map(|(c, _)| (*c).to_string()).collect();
        let entry = next_binding_entry(
            Template::Xbox,
            "A",
            "gamepad.buttons",
            XMB_ACTIONS,
            &all_used,
        );
        assert_eq!(
            entry, None,
            "must not produce empty or duplicate binding when all buttons are bound"
        );
    }

    #[test]
    fn ds4_touchpad_unmapped_falls_back_in_order() {
        // Touchpad has code: None. next_binding_entry should find the first hotspot with a code in DS4 order.
        let entry = next_binding_entry(
            Template::Ds4,
            "Touchpad",
            "gamepad.buttons",
            XMB_ACTIONS,
            &[],
        );
        assert!(entry.is_some());
        let (code, action) = entry.unwrap();
        assert!(!code.is_empty());
        assert!(XMB_ACTIONS.contains(&action));
    }

    #[test]
    fn ds4_artwork_has_no_visible_touchpad_text_but_keeps_hotspot() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ds4_svg = fs::read_to_string(manifest_dir.join("../../assets/controller-ds4.svg"))
            .expect("controller-ds4.svg missing");

        // The touchpad rectangle element must remain present for layout and hotspot alignment
        assert!(
            ds4_svg.contains("id=\"Touchpad\""),
            "Touchpad rect must exist"
        );
        // But visible text must not be rendered in the controller artwork
        assert!(
            !ds4_svg.contains(">TOUCHPAD<") && !ds4_svg.contains(">Touchpad<"),
            "DS4 artwork must not display visible Touchpad text"
        );

        // Hotspot selection and behavior remains intact
        let ds4_hotspots = Template::Ds4.hotspots();
        let touchpad_hs = ds4_hotspots
            .iter()
            .find(|h| h.control == "Touchpad")
            .expect("Touchpad hotspot must exist in DS4 template");
        assert_eq!(touchpad_hs.code, None);
        assert!(!touchpad_hs.label);
    }

    #[test]
    fn binding_editor_context_uses_correct_action_set() {
        // Context "xmb" must use XMB_ACTIONS
        assert!(XMB_ACTIONS.contains(&"confirm"));
        assert!(!XMB_ACTIONS.contains(&"pointer.left"));

        // Context "base" or layers must use ACTIONS
        assert!(ACTIONS.contains(&"pointer.left"));
        assert!(!ACTIONS.contains(&"confirm"));
    }

    #[test]
    fn controller_tab_navigation_and_quick_map_actions() {
        assert_eq!(ControllerTab::default(), ControllerTab::Bindings);
        assert_eq!(ControllerTab::Bindings.label(), "Bindings & Layers");
        assert_eq!(ControllerTab::Curves.label(), "Analog & Curves");
        assert_eq!(ControllerTab::Diagnostics.label(), "Diagnostics");
        assert_eq!(ControllerTab::Device.label(), "Device Settings");

        // Discrete mouse and scroll actions must all be valid in ACTIONS
        for mouse_action in [
            "pointer.left",
            "pointer.right",
            "pointer.middle",
            "pointer.back",
            "pointer.forward",
            "pointer.move-up",
            "pointer.move-down",
            "pointer.move-left",
            "pointer.move-right",
            "pointer.scroll-up",
            "pointer.scroll-down",
        ] {
            assert!(
                ACTIONS.contains(&mouse_action),
                "action {mouse_action} must be selectable for any button"
            );
        }

        // Verify that D-pad and face buttons are recognized buttons
        let button_codes: Vec<&str> = BUTTONS.iter().map(|(code, _)| *code).collect();
        assert!(button_codes.contains(&"BTN_DPAD_UP"));
        assert!(button_codes.contains(&"BTN_DPAD_DOWN"));
        assert!(button_codes.contains(&"BTN_DPAD_LEFT"));
        assert!(button_codes.contains(&"BTN_DPAD_RIGHT"));
        assert!(button_codes.contains(&"BTN_SOUTH"));
        assert!(button_codes.contains(&"BTN_EAST"));
        assert!(button_codes.contains(&"BTN_TRIGGER_HAPPY1"));
    }

    #[test]
    fn pointer_and_scroll_navigation_pods_and_stick_controls() {
        // Stick detection on Xbox
        assert!(is_left_stick("L STICK"));
        assert!(!is_right_stick("L STICK"));

        assert!(!is_left_stick("R STICK"));
        assert!(is_right_stick("R STICK"));

        // Stick detection on DS4
        assert!(is_left_stick("L3"));
        assert!(!is_right_stick("L3"));

        assert!(!is_left_stick("R3"));
        assert!(is_right_stick("R3"));

        // Stick template mapping
        assert_eq!(stick_control_for_template(Template::Ds4, "left"), "L3");
        assert_eq!(stick_control_for_template(Template::Ds4, "right"), "R3");
        assert_eq!(
            stick_control_for_template(Template::Xbox, "left"),
            "L STICK"
        );
        assert_eq!(
            stick_control_for_template(Template::Xbox, "right"),
            "R STICK"
        );

        // Control source labels
        assert_eq!(ControlSource::LeftStick.label(), "Left Stick");
        assert_eq!(ControlSource::RightStick.label(), "Right Stick");
        assert_eq!(ControlSource::Dpad.label(), "D-pad");
        assert_eq!(ControlSource::Other.label(), "Other");

        // Control name for code
        assert_eq!(
            control_name_for_code_and_template(Template::Xbox, "BTN_SOUTH"),
            "A"
        );
        assert_eq!(
            control_name_for_code_and_template(Template::Xbox, "BTN_DPAD_UP"),
            "D-pad up"
        );
        assert_eq!(
            control_name_for_code_and_template(Template::Xbox, "BTN_TRIGGER_HAPPY1"),
            "Paddle 1"
        );
        assert_eq!(
            control_name_for_code_and_template(Template::Ds4, "BTN_SOUTH"),
            "Cross"
        );
        assert_eq!(
            control_name_for_code_and_template(Template::Ds4, "BTN_TL"),
            "L1"
        );
    }
}
