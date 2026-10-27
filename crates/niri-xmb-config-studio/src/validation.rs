//! Inline, per-field validation for the studio model.
//!
//! The daemon is the single source of truth: a model that parses as
//! [`Config`] is checked with `Config::validate_all`, and each daemon
//! message is mapped back to the studio's dotted field path so the error
//! can be shown under the input that caused it. Free-text fields the
//! daemon would reject while parsing (colours, lengths, output names,
//! device paths, axis names) are also checked directly, so a parse
//! failure in one field never hides an error in another.

use niri_xmb::config::Config;
use niri_xmb::draw::StyleColor;
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq)]
pub struct FieldError {
    /// Studio dotted path, e.g. `bars.0.output`. Empty means model-wide.
    pub path: String,
    pub message: String,
}

impl FieldError {
    fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Length fields and the daemon ranges for them: `(pixels, percent)`.
fn length_range(path: &str) -> ((f64, f64), (f64, f64)) {
    if path == "popup.fixed_x" || path == "popup.fixed_y" {
        ((-8192.0, 8192.0), (-100.0, 100.0))
    } else if path.ends_with(".margin") {
        ((0.0, 8192.0), (0.0, 100.0))
    } else {
        ((1.0, 8192.0), (0.1, 100.0))
    }
}

fn length_error(path: &str, value: &Value) -> Option<String> {
    let (pixels, percent) = length_range(path);
    match value {
        Value::Number(number) => {
            let value = number.as_f64()?;
            if !value.is_finite() || value < pixels.0 || value > pixels.1 {
                Some(format!(
                    "{path} must be finite and between {} and {}; got {value}",
                    pixels.0, pixels.1
                ))
            } else {
                None
            }
        }
        Value::String(text) => {
            let Some(raw) = text.trim().strip_suffix('%') else {
                return Some(format!(
                    "{path} must be a number or a percentage such as \"25%\"; got {text:?}"
                ));
            };
            match raw.trim().parse::<f64>() {
                Ok(value) if value.is_finite() && value >= percent.0 && value <= percent.1 => None,
                Ok(value) => Some(format!(
                    "{path} must be finite and between {} and {}; got {value}",
                    percent.0, percent.1
                )),
                Err(_) => Some(format!(
                    "{path} must be a number or a percentage such as \"25%\"; got {text:?}"
                )),
            }
        }
        _ => Some(format!(
            "{path} must be a number or a percentage such as \"25%\""
        )),
    }
}

fn output_error(value: &str) -> Option<&'static str> {
    if value.is_empty() || value.trim() != value {
        Some("must be a nonempty output name without surrounding whitespace")
    } else if value.len() > 255 || value.chars().any(char::is_control) {
        Some("must be at most 255 bytes and contain no control characters")
    } else {
        None
    }
}

fn get<'a>(model: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = model;
    for part in path.split('.') {
        if let Ok(index) = part.parse::<usize>() {
            current = current.as_array()?.get(index)?;
        } else {
            current = current.as_object()?.get(part)?;
        }
    }
    Some(current)
}

fn push_unique(errors: &mut Vec<FieldError>, error: FieldError) {
    if !errors
        .iter()
        .any(|existing| existing.path == error.path && existing.message == error.message)
    {
        errors.push(error);
    }
}

/// Convert a daemon field name (`bars[0].output`, `gamepad.devices[2]`)
/// to the studio's dotted path (`bars.0.output`, `gamepad.devices.2`).
fn studio_path(daemon_field: &str) -> String {
    daemon_field.replace('[', ".").replace(']', "")
}

/// The daemon field a message is about: its first whitespace-delimited
/// token, minus any trailing punctuation added by context chains.
fn error_path(message: &str) -> String {
    let token = message.split_whitespace().next().unwrap_or("");
    let token = token.trim_end_matches([':', ',']);
    // Context chains join with ": "; a valid field never contains ':'.
    let token = token.split(':').next().unwrap_or(token);
    studio_path(token)
}

pub fn field_errors(model: &Value) -> Vec<FieldError> {
    let mut errors: Vec<FieldError> = Vec::new();
    if model.is_null() {
        return errors;
    }

    // Output names: bars and popup placements.
    for (list_path, daemon_list) in [("bars", "bars"), ("popup.placements", "popup.placements")] {
        let mut seen: HashSet<&str> = HashSet::new();
        if let Some(items) = get(model, list_path).and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                let path = format!("{list_path}.{index}.output");
                let output = item.get("output").and_then(Value::as_str).unwrap_or("");
                if let Some(reason) = output_error(output) {
                    push_unique(
                        &mut errors,
                        FieldError::new(
                            path.clone(),
                            format!("{daemon_list}[{index}].output {reason}"),
                        ),
                    );
                } else if !seen.insert(output) {
                    push_unique(
                        &mut errors,
                        FieldError::new(
                            path,
                            format!("{daemon_list}[{index}].output duplicates output {output:?}"),
                        ),
                    );
                }
            }
        }
    }

    // Numeric bar limits are checked here as well as by the daemon. Keeping
    // these checks independent means one malformed output name cannot hide
    // errors in the same bar.
    if let Some(bars) = get(model, "bars").and_then(Value::as_array) {
        for (index, bar) in bars.iter().enumerate() {
            let prefix = format!("bars.{index}");
            let horizontal = matches!(
                bar.get("edge").and_then(Value::as_str),
                Some("top" | "bottom")
            );
            let width_max = if horizontal { 8192.0 } else { 512.0 };
            let height_max = if horizontal { 512.0 } else { 8192.0 };
            for (field, min, max) in [("width", 1.0, width_max), ("height", 1.0, height_max)] {
                if let Some(value) = bar.get(field).and_then(Value::as_f64)
                    && (!value.is_finite() || !(min..=max).contains(&value))
                {
                    push_unique(
                        &mut errors,
                        FieldError::new(
                            format!("{prefix}.{field}"),
                            format!(
                                "bars[{index}].{field} must be between {min} and {max}; got {value}"
                            ),
                        ),
                    );
                }
            }
            for (field, min, max) in [("margin", 0.0, 8192.0), ("offset", -8192.0, 8192.0)] {
                if let Some(value) = bar.get(field).and_then(Value::as_f64)
                    && (!value.is_finite() || !(min..=max).contains(&value))
                {
                    push_unique(
                        &mut errors,
                        FieldError::new(
                            format!("{prefix}.{field}"),
                            format!(
                                "bars[{index}].{field} is outside the allowed range; got {value}"
                            ),
                        ),
                    );
                }
            }
        }
    }

    if let Some(value) = get(model, "animation.open_scale").and_then(Value::as_f64)
        && (!value.is_finite() || !(0.1..=2.0).contains(&value))
    {
        push_unique(
            &mut errors,
            FieldError::new(
                "animation.open_scale",
                format!("animation.open_scale must be between 0.1 and 2.0; got {value}"),
            ),
        );
    }

    // Theme: empty is the only free-text failure; a missing/null theme is fine.
    if let Some(theme) = get(model, "theme").and_then(Value::as_str)
        && theme.trim().is_empty()
    {
        push_unique(
            &mut errors,
            FieldError::new("theme", "theme must not be empty"),
        );
    }

    // Colours.
    for path in COLOR_PATHS {
        if let Some(text) = get(model, path).and_then(Value::as_str)
            && let Err(reason) = StyleColor::parse(text)
        {
            push_unique(
                &mut errors,
                FieldError::new(*path, format!("style colour {reason}")),
            );
        }
    }

    // Lengths.
    for path in LENGTH_PATHS {
        if let Some(value) = get(model, path)
            && let Some(message) = length_error(path, value)
        {
            push_unique(&mut errors, FieldError::new(*path, message));
        }
    }
    if let Some(items) = get(model, "popup.placements").and_then(Value::as_array) {
        for index in 0..items.len() {
            let path = format!("popup.placements.{index}.margin");
            if let Some(value) = get(model, &path)
                && let Some(message) = length_error(&path, value)
            {
                push_unique(&mut errors, FieldError::new(path, message));
            }
        }
    }

    // Devices: each entry must be a nonempty absolute path.
    if let Some(devices) = get(model, "gamepad.devices").and_then(Value::as_array) {
        for (index, device) in devices.iter().enumerate() {
            let text = device.as_str().unwrap_or("");
            if text.trim().is_empty() {
                push_unique(
                    &mut errors,
                    FieldError::new(
                        "gamepad.devices",
                        format!("gamepad.devices[{index}] must not be empty"),
                    ),
                );
            } else if !text.trim().starts_with('/') {
                push_unique(
                    &mut errors,
                    FieldError::new(
                        "gamepad.devices",
                        format!(
                            "gamepad.devices[{index}] must be an absolute path: {}",
                            text.trim()
                        ),
                    ),
                );
            }
        }
    }

    // Layer hold axes: free-text axis names.
    if let Some(layers) = get(model, "gamepad.navi.layers").and_then(Value::as_object) {
        for (name, layer) in layers {
            let hold_empty = layer
                .get("hold")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty);
            let axes = layer.get("hold_axes").and_then(Value::as_array);
            if hold_empty && axes.is_none_or(Vec::is_empty) {
                push_unique(
                    &mut errors,
                    FieldError::new(
                        format!("gamepad.navi.layers.{name}"),
                        format!("gamepad.navi.layers.{name} must define hold or hold_axes"),
                    ),
                );
            }
            if let Some(axes) = axes {
                let mut seen = HashSet::new();
                for (index, axis) in axes.iter().enumerate() {
                    let path = format!("gamepad.navi.layers.{name}.hold_axes.{index}.axis");
                    let text = axis.get("axis").and_then(Value::as_str).unwrap_or("");
                    if !is_axis_name(text) {
                        push_unique(
                            &mut errors,
                            FieldError::new(
                                path,
                                format!(
                                    "unknown gamepad axis {text:?}; use an ABS name or a 16-bit code"
                                ),
                            ),
                        );
                    } else if !seen.insert(text) {
                        push_unique(
                            &mut errors,
                            FieldError::new(
                                format!("gamepad.navi.layers.{name}.hold_axes"),
                                format!(
                                    "gamepad.navi.layers.{name}.hold_axes contains duplicate axis {text:?}"
                                ),
                            ),
                        );
                    }
                }
            }
        }
    }

    // Daemon semantic validation, when the model parses at all.
    match serde_json::from_value::<Config>(model.clone()) {
        Ok(config) => {
            for message in config.validate_all() {
                let path = error_path(&message);
                // Free-text checks above already cover these paths with the
                // same wording; keep one copy per path.
                if errors.iter().any(|error| error.path == path) {
                    continue;
                }
                push_unique(&mut errors, FieldError::new(path, message));
            }
        }
        Err(parse_error) => {
            // A field-level check above should explain this; if none does,
            // keep the failure visible instead of letting Save discover it.
            let explained = !errors.is_empty();
            if !explained {
                push_unique(
                    &mut errors,
                    FieldError::new("", format!("invalid configuration: {parse_error}")),
                );
            }
        }
    }
    errors
}

fn is_axis_name(text: &str) -> bool {
    let text = text.trim();
    if matches!(
        text,
        "ABS_X" | "ABS_Y" | "ABS_Z" | "ABS_RX" | "ABS_RY" | "ABS_RZ"
    ) {
        return true;
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u16::from_str_radix(hex, 16).is_ok();
    }
    text.parse::<u16>().is_ok()
}

const COLOR_PATHS: &[&str] = &[
    "style.background",
    "style.background_end",
    "style.surface",
    "style.surface_hover",
    "style.border",
    "style.border_soft",
    "style.accent",
    "style.selection_border",
    "style.text",
    "style.muted",
    "style.urgent",
    "style.shadow",
    "style.floating_backing",
];

const LENGTH_PATHS: &[&str] = &[
    "popup.width",
    "popup.height",
    "popup.canvas_width",
    "popup.canvas_height",
    "popup.fixed_x",
    "popup.fixed_y",
];

/// First live output not already used, else the first unused conventional
/// connector name. New bars/placements are seeded with this so they are
/// valid on creation instead of failing at save time.
pub fn output_seed(used: &[String], live_outputs: &[String]) -> String {
    for candidate in live_outputs {
        if !used.iter().any(|existing| existing == candidate) {
            return candidate.clone();
        }
    }
    for candidate in ["eDP-1", "HDMI-A-1", "DP-1", "HDMI-A-2", "DP-2"] {
        if !used.iter().any(|existing| existing == candidate) {
            return candidate.to_string();
        }
    }
    let mut index = 1;
    loop {
        let candidate = format!("eDP-{index}");
        if !used.contains(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

/// A referenced theme that cannot be read fails daemon load, so flag it on
/// the Theme field instead of at save time. Mirrors the daemon's path
/// resolution: `~/` expands to `$HOME`, relative paths resolve against the
/// config file's directory.
pub fn theme_error(theme: &str, config_path: &str) -> Option<String> {
    if theme.trim().is_empty() {
        return Some("theme must not be empty".to_string());
    }
    let resolved = if let Some(rest) = theme.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| std::path::PathBuf::from(home).join(rest))
            .unwrap_or_else(|| std::path::PathBuf::from(theme))
    } else {
        let candidate = std::path::PathBuf::from(theme);
        if candidate.is_absolute() {
            candidate
        } else {
            std::path::Path::new(config_path)
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join(candidate)
        }
    };
    if resolved.exists() {
        None
    } else {
        Some(format!("theme file not found: {}", resolved.display()))
    }
}

pub fn default_bar(output: String) -> Value {
    json!({
        "output": output,
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

pub fn default_placement(output: String) -> Value {
    json!({"output": output, "x": "center", "y": "center", "margin": 48})
}

/// A new layer must satisfy `must define hold or hold_axes` on creation.
pub fn default_layer() -> Value {
    json!({"hold": [], "hold_axes": [{"axis": "ABS_Z", "threshold": 0.35}], "bindings": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn errors_for(model: Value) -> Vec<FieldError> {
        field_errors(&model)
    }

    fn paths(errors: &[FieldError]) -> Vec<&str> {
        errors.iter().map(|error| error.path.as_str()).collect()
    }

    #[test]
    fn empty_bar_output_is_flagged_on_its_own_field() {
        let model = json!({"bars": [default_bar(String::new())]});
        let errors = errors_for(model);
        assert!(
            errors.iter().any(|error| error.path == "bars.0.output"
                && error.message.contains("nonempty output name")),
            "{errors:?}"
        );
    }

    #[test]
    fn seeded_bar_and_placement_are_valid() {
        let model = json!({
            "bars": [default_bar(output_seed(&[], &["HDMI-A-1".into()]))],
            "popup": {"placements": [default_placement(output_seed(&[], &[]))]}
        });
        let errors = errors_for(model);
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn duplicate_outputs_are_flagged_on_the_second_item() {
        let model = json!({"bars": [default_bar("eDP-1".into()), default_bar("eDP-1".into())]});
        let errors = errors_for(model);
        assert!(paths(&errors).contains(&"bars.1.output"), "{errors:?}");
    }

    #[test]
    fn every_bad_field_is_reported_not_just_the_first() {
        let mut bar = default_bar(String::new());
        bar["width"] = json!(9000);
        let model = json!({
            "bars": [bar],
            "popup": {"width": "mostly"},
            "style": {"background": "red"},
            "animation": {"open_scale": 0.0}
        });
        let errors = errors_for(model);
        for expected in [
            "bars.0.output",
            "bars.0.width",
            "popup.width",
            "style.background",
            "animation.open_scale",
        ] {
            assert!(
                paths(&errors).contains(&expected),
                "missing {expected}: {errors:?}"
            );
        }
    }

    #[test]
    fn invalid_colour_does_not_hide_output_error() {
        let model = json!({
            "bars": [default_bar(String::new())],
            "style": {"background": "red"}
        });
        let errors = errors_for(model);
        assert!(paths(&errors).contains(&"bars.0.output"), "{errors:?}");
        assert!(paths(&errors).contains(&"style.background"), "{errors:?}");
    }

    #[test]
    fn length_kinds_use_their_daemon_ranges() {
        let model = json!({"popup": {"width": "0%", "fixed_x": "-100%", "placements": [{"output": "eDP-1", "margin": "101%"}]}});
        let errors = errors_for(model);
        assert!(paths(&errors).contains(&"popup.width"), "{errors:?}");
        assert!(!paths(&errors).contains(&"popup.fixed_x"), "{errors:?}");
        assert!(
            paths(&errors).contains(&"popup.placements.0.margin"),
            "{errors:?}"
        );
    }

    #[test]
    fn output_seed_skips_used_names() {
        assert_eq!(output_seed(&["eDP-1".into()], &[]), "HDMI-A-1");
        assert_eq!(
            output_seed(&["HDMI-A-1".into()], &["HDMI-A-1".into(), "DP-3".into()]),
            "DP-3"
        );
    }

    #[test]
    fn default_layer_is_valid() {
        let model = json!({"gamepad": {"navi": {"layers": {"layer": default_layer()}}}});
        let errors = errors_for(model);
        assert!(
            !errors
                .iter()
                .any(|error| error.path.starts_with("gamepad.navi.layers")),
            "{errors:?}"
        );
    }

    #[test]
    fn missing_theme_file_is_flagged() {
        assert!(
            theme_error("does-not-exist.toml", "/tmp/niri-xmb-test/config.toml")
                .is_some_and(|message| message.contains("not found"))
        );
        assert!(theme_error("", "/tmp/config.toml").is_some());
    }

    #[test]
    fn relative_device_and_bad_axis_are_flagged() {
        let model = json!({
            "gamepad": {
                "devices": ["relative/path"],
                "navi": {"layers": {"x": {"hold": [], "hold_axes": [{"axis": "NOPE", "threshold": 0.5}], "bindings": {}}}}
            }
        });
        let errors = errors_for(model);
        assert!(paths(&errors).contains(&"gamepad.devices"), "{errors:?}");
        assert!(
            paths(&errors).contains(&"gamepad.navi.layers.x.hold_axes.0.axis"),
            "{errors:?}"
        );
    }
}
