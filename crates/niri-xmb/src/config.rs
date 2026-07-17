use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bars: Vec<BarConfig>,
    pub popup: PopupConfig,
    pub style: StyleConfig,
    pub behavior: BehaviorConfig,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&source).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("niri-xmb/config.toml")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BarConfig {
    /// Niri connector name, for example `eDP-1` or `HDMI-A-1`.
    pub output: String,
    pub edge: Edge,
    pub align: Align,
    pub width: u32,
    pub height: u32,
    pub margin: i32,
    pub offset: i32,
    pub allow_column_join: bool,
    pub show_workspace_labels: bool,
    pub hide_empty_workspaces: bool,
    pub show_capsule: bool,
    pub show_active_indicator: bool,
    pub scroll_to_switch: bool,
    pub scroll_wrap: bool,
    pub scroll_reverse: bool,
}

impl Default for BarConfig {
    fn default() -> Self {
        Self {
            output: String::new(),
            edge: Edge::Top,
            align: Align::Start,
            width: 420,
            height: 38,
            margin: 4,
            offset: 8,
            allow_column_join: false,
            show_workspace_labels: false,
            hide_empty_workspaces: false,
            show_capsule: true,
            show_active_indicator: true,
            scroll_to_switch: true,
            scroll_wrap: true,
            scroll_reverse: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PopupConfig {
    pub outputs: PopupOutputs,
    pub surface_mode: PopupSurfaceMode,
    pub follow_delay_ms: u64,
    pub position: PopupPosition,
    pub width: u32,
    pub height: u32,
    pub fixed_x: i32,
    pub fixed_y: i32,
    pub show_titles: bool,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            outputs: PopupOutputs::Focused,
            surface_mode: PopupSurfaceMode::FollowActive,
            follow_delay_ms: 180,
            position: PopupPosition::Cursor,
            width: 920,
            height: 250,
            fixed_x: 80,
            fixed_y: 80,
            show_titles: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PopupOutputs {
    Focused,
    All,
    Combined,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PopupSurfaceMode {
    Fixed,
    FollowActive,
    Mirror,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PopupPosition {
    Cursor,
    Fixed,
    Center,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    pub background: String,
    pub surface: String,
    pub surface_hover: String,
    pub border: String,
    pub border_soft: String,
    pub accent: String,
    pub selection_border: String,
    pub text: String,
    pub muted: String,
    pub urgent: String,
    pub radius: f32,
    pub border_width: f32,
    pub icon_size: u32,
    pub gap: f32,
}

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            background: "#f5111415".into(),
            surface: "#661d2022".into(),
            surface_hover: "#266ed2ff".into(),
            border: "#ff40484c".into(),
            border_soft: "#3340484c".into(),
            accent: "#ff6ed2ff".into(),
            selection_border: "#666ed2ff".into(),
            text: "#ffe1e2e5".into(),
            muted: "#ffc0c7cd".into(),
            urgent: "#ffffb4ab".into(),
            radius: 14.0,
            border_width: 1.0,
            icon_size: 28,
            gap: 8.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    pub drag_threshold: f64,
    pub empty_drop_floats: bool,
    pub floating_drop_tiles: bool,
    pub focus_after_drop: bool,
    pub close_on_window_click: bool,
    pub close_on_workspace_click: bool,
    pub close_on_drop: bool,
    pub close_on_outside_click: bool,
    pub close_on_escape: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            drag_threshold: 7.0,
            empty_drop_floats: false,
            floating_drop_tiles: true,
            focus_after_drop: false,
            close_on_window_click: true,
            close_on_workspace_click: false,
            close_on_drop: false,
            close_on_outside_click: true,
            close_on_escape: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips() {
        let encoded = toml::to_string_pretty(&Config::default()).unwrap();
        toml::from_str::<Config>(&encoded).unwrap();
    }

    #[test]
    fn shipped_example_config_parses() {
        let source = include_str!("../../../config/niri-xmb.toml.example");
        let config = toml::from_str::<Config>(source).unwrap();
        assert_eq!(config.bars.len(), 2);
        assert!(!config.bars[0].allow_column_join);
        assert!(config.bars[1].allow_column_join);
    }
}
