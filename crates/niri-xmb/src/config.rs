use crate::draw::StyleColor;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

const MAX_LOGICAL_DIMENSION: f32 = 8192.0;
const MAX_OFFSET: f32 = 8192.0;
const MAX_DURATION_MS: u64 = 60_000;
const MAX_STYLE_LENGTH: f32 = 2048.0;
const MAX_FONT_SIZE: f32 = 512.0;
const MAX_ICON_SIZE: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bars_enabled: bool,
    pub bars: Vec<BarConfig>,
    pub popup: PopupConfig,
    pub peek: PeekConfig,
    pub animation: AnimationConfig,
    pub style: StyleConfig,
    pub behavior: BehaviorConfig,
    /// Path to a theme file whose `[style]` table is merged over this one's,
    /// key by key. A leading `~/` expands to `$HOME`; a relative path is
    /// resolved against the directory containing this config file.
    pub theme: Option<String>,
}

impl PopupConfig {
    /// Anchor for `output`, or a centred default when it has no entry.
    pub fn placement(&self, output: &str) -> PopupPlacement {
        self.placements
            .iter()
            .find(|placement| placement.output == output)
            .cloned()
            .unwrap_or_default()
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            let config = Self::default();
            config.validate()?;
            return Ok(config);
        }
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut value: toml::Value = toml::from_str(&source)
            .with_context(|| format!("failed to parse {}", path.display()))?;

        let theme_path = value
            .get("theme")
            .and_then(toml::Value::as_str)
            .map(str::to_owned);
        if let Some(theme) = theme_path {
            if theme.trim().is_empty() {
                anyhow::bail!("theme must not be empty");
            }
            let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
            let resolved = resolve_theme_path(&theme, config_dir);
            let theme_source = std::fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read theme {}", resolved.display()))?;
            let theme_value: toml::Value = toml::from_str(&theme_source)
                .with_context(|| format!("failed to parse theme {}", resolved.display()))?;

            validate_theme(&theme_value, &resolved)?;

            let table = value
                .as_table_mut()
                .expect("top-level config is always a table");
            let style = table
                .entry("style")
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            let theme_style = theme_value
                .get("style")
                .expect("validated theme has a style table");
            merge_style(style, theme_style);
        }

        let mut config: Self = value
            .try_into()
            .with_context(|| format!("failed to parse {}", path.display()))?;
        config.reconcile_legacy_switches();
        config
            .validate()
            .with_context(|| format!("invalid configuration in {}", path.display()))?;
        Ok(config)
    }

    /// Apply deprecated switches without silently changing behavior.
    ///
    /// `show_urgent_ring = false` used to suppress the `ring` preset. That
    /// combination keeps its meaning by mapping to `urgent_indicator = "none"`;
    /// every other use of the switch is now ignored.
    pub fn reconcile_legacy_switches(&mut self) {
        if self.style.show_urgent_ring == Some(false)
            && self.style.urgent_indicator == UrgentIndicator::Ring
        {
            tracing::warn!(
                "style.show_urgent_ring is deprecated; replace `urgent_indicator = \"ring\"` \
                 plus `show_urgent_ring = false` with `urgent_indicator = \"none\"`"
            );
            self.style.urgent_indicator = UrgentIndicator::None;
        }
        self.style.show_urgent_ring = None;
    }

    /// Validate values that TOML and Serde cannot constrain by type.
    ///
    /// Logical pixel dimensions are limited to 8192. Durations are limited to
    /// 60 seconds. Percent sizes use 0-100, while signed positions use
    /// -100-100. These limits prevent accidental large buffers and timers.
    pub fn validate(&self) -> Result<()> {
        let mut bar_outputs = HashSet::new();
        for (index, bar) in self.bars.iter().enumerate() {
            validate_output_name(&bar.output, &format!("bars[{index}].output"))?;
            if !bar_outputs.insert(&bar.output) {
                anyhow::bail!("bars[{index}].output duplicates output {:?}", bar.output);
            }
            let (max_width, max_height) = match bar.edge {
                Edge::Top | Edge::Bottom => (8192, 512),
                Edge::Left | Edge::Right => (512, 8192),
            };
            validate_u32_range(&format!("bars[{index}].width"), bar.width, 1, max_width)?;
            validate_u32_range(&format!("bars[{index}].height"), bar.height, 1, max_height)?;
            validate_i32_range(
                &format!("bars[{index}].margin"),
                bar.margin,
                0,
                MAX_OFFSET as i32,
            )?;
            validate_i32_range(
                &format!("bars[{index}].offset"),
                bar.offset,
                -(MAX_OFFSET as i32),
                MAX_OFFSET as i32,
            )?;
            validate_u64_range(
                &format!("bars[{index}].scroll_repeat_ms"),
                bar.scroll_repeat_ms,
                0,
                5_000,
            )?;
        }

        validate_length(
            "popup.width",
            self.popup.width,
            1.0,
            MAX_LOGICAL_DIMENSION,
            0.1,
            100.0,
        )?;
        validate_length(
            "popup.height",
            self.popup.height,
            1.0,
            MAX_LOGICAL_DIMENSION,
            0.1,
            100.0,
        )?;
        validate_length(
            "popup.canvas_width",
            self.popup.canvas_width,
            1.0,
            MAX_LOGICAL_DIMENSION,
            0.1,
            100.0,
        )?;
        validate_length(
            "popup.canvas_height",
            self.popup.canvas_height,
            1.0,
            MAX_LOGICAL_DIMENSION,
            0.1,
            100.0,
        )?;
        validate_length(
            "popup.fixed_x",
            self.popup.fixed_x,
            -MAX_OFFSET,
            MAX_OFFSET,
            -100.0,
            100.0,
        )?;
        validate_length(
            "popup.fixed_y",
            self.popup.fixed_y,
            -MAX_OFFSET,
            MAX_OFFSET,
            -100.0,
            100.0,
        )?;
        validate_u64_range(
            "popup.follow_delay_ms",
            self.popup.follow_delay_ms,
            0,
            MAX_DURATION_MS,
        )?;

        let mut placement_outputs = HashSet::new();
        for (index, placement) in self.popup.placements.iter().enumerate() {
            validate_output_name(
                &placement.output,
                &format!("popup.placements[{index}].output"),
            )?;
            if !placement_outputs.insert(&placement.output) {
                anyhow::bail!(
                    "popup.placements[{index}].output duplicates output {:?}",
                    placement.output
                );
            }
            validate_length(
                &format!("popup.placements[{index}].margin"),
                placement.margin,
                0.0,
                MAX_OFFSET,
                0.0,
                100.0,
            )?;
        }

        for (field, value) in [
            ("peek.duration_ms", self.peek.duration_ms),
            ("peek.arm_grace_ms", self.peek.arm_grace_ms),
            ("peek.demote_ms", self.peek.demote_ms),
        ] {
            validate_u64_range(field, value, 0, MAX_DURATION_MS)?;
        }
        validate_f32_range("animation.panel_ms", self.animation.panel_ms, 0.0, 60_000.0)?;
        validate_f32_range(
            "animation.emphasis_ms",
            self.animation.emphasis_ms,
            0.0,
            60_000.0,
        )?;
        validate_f32_range(
            "animation.scroll_ms",
            self.animation.scroll_ms,
            0.0,
            60_000.0,
        )?;
        validate_f32_range("animation.open_scale", self.animation.open_scale, 0.1, 2.0)?;
        validate_f64_range(
            "behavior.drag_threshold",
            self.behavior.drag_threshold,
            0.0,
            MAX_LOGICAL_DIMENSION as f64,
        )?;
        validate_f64_range(
            "behavior.popup_grab_region",
            self.behavior.popup_grab_region,
            0.0,
            MAX_LOGICAL_DIMENSION as f64,
        )?;
        self.style.validate()?;
        if self
            .theme
            .as_ref()
            .is_some_and(|theme| theme.trim().is_empty())
        {
            anyhow::bail!("theme must not be empty");
        }
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("niri-xmb/config.toml")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bars_enabled: true,
            bars: Vec::new(),
            popup: PopupConfig::default(),
            peek: PeekConfig::default(),
            animation: AnimationConfig::default(),
            style: StyleConfig::default(),
            behavior: BehaviorConfig::default(),
            theme: None,
        }
    }
}

/// Resolve a theme path relative to the config file that referenced it.
///
/// A leading `~/` expands to `$HOME`; an absolute path is used as-is; anything
/// else is resolved relative to `config_dir`.
fn resolve_theme_path(theme: &str, config_dir: &Path) -> PathBuf {
    if let Some(rest) = theme.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let candidate = PathBuf::from(theme);
    if candidate.is_absolute() {
        candidate
    } else {
        config_dir.join(candidate)
    }
}

/// Merge a theme's `[style]` table into the base config's `[style]` table,
/// key by key, with the theme's values winning. Keys the theme does not set
/// are left untouched in `base`, so a theme overriding only a few colours
/// does not reset the rest of the style fields to defaults.
fn merge_style(base: &mut toml::Value, theme: &toml::Value) {
    let (Some(base_table), Some(theme_table)) = (base.as_table_mut(), theme.as_table()) else {
        return;
    };
    for (key, value) in theme_table {
        base_table.insert(key.clone(), value.clone());
    }
}

fn validate_theme(theme: &toml::Value, path: &Path) -> Result<()> {
    let table = theme
        .as_table()
        .with_context(|| format!("theme {} must contain a [style] table", path.display()))?;
    let style = table
        .get("style")
        .with_context(|| format!("theme {} is missing required [style] table", path.display()))?;
    if let Some(key) = table.keys().find(|key| key.as_str() != "style") {
        anyhow::bail!(
            "theme {} has unsupported top-level key {key:?}; only [style] is allowed",
            path.display()
        );
    }
    let style_table = style
        .as_table()
        .with_context(|| format!("theme {} field style must be a table", path.display()))?;

    let mut merged = toml::Value::try_from(StyleConfig::default())
        .context("failed to build default style for theme validation")?;
    let known = merged
        .as_table()
        .expect("serialized StyleConfig is a table")
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    if let Some(key) = style_table.keys().find(|key| !known.contains(*key)) {
        anyhow::bail!("theme {} has unknown style key style.{key}", path.display());
    }
    merge_style(&mut merged, style);
    let parsed: StyleConfig = merged
        .try_into()
        .with_context(|| format!("theme {} has an invalid [style] field", path.display()))?;
    parsed
        .validate()
        .with_context(|| format!("invalid theme {}", path.display()))
}

fn validate_output_name(value: &str, field: &str) -> Result<()> {
    if value.is_empty() || value.trim() != value {
        anyhow::bail!("{field} must be a nonempty output name without surrounding whitespace");
    }
    if value.len() > 255 || value.chars().any(char::is_control) {
        anyhow::bail!("{field} must be at most 255 bytes and contain no control characters");
    }
    Ok(())
}

fn validate_length(
    field: &str,
    value: ConfigLength,
    min_pixels: f32,
    max_pixels: f32,
    min_percent: f32,
    max_percent: f32,
) -> Result<()> {
    match value {
        ConfigLength::Pixels(value) => validate_f32_range(field, value, min_pixels, max_pixels),
        ConfigLength::Percent(value) => validate_f32_range(field, value, min_percent, max_percent),
    }
}

fn validate_f32_range(field: &str, value: f32, min: f32, max: f32) -> Result<()> {
    if !value.is_finite() || value < min || value > max {
        anyhow::bail!("{field} must be finite and between {min} and {max}; got {value}");
    }
    Ok(())
}

fn validate_f64_range(field: &str, value: f64, min: f64, max: f64) -> Result<()> {
    if !value.is_finite() || value < min || value > max {
        anyhow::bail!("{field} must be finite and between {min} and {max}; got {value}");
    }
    Ok(())
}

fn validate_u32_range(field: &str, value: u32, min: u32, max: u32) -> Result<()> {
    if value < min || value > max {
        anyhow::bail!("{field} must be between {min} and {max}; got {value}");
    }
    Ok(())
}

fn validate_i32_range(field: &str, value: i32, min: i32, max: i32) -> Result<()> {
    if value < min || value > max {
        anyhow::bail!("{field} must be between {min} and {max}; got {value}");
    }
    Ok(())
}

fn validate_u64_range(field: &str, value: u64, min: u64, max: u64) -> Result<()> {
    if value < min || value > max {
        anyhow::bail!("{field} must be between {min} and {max}; got {value}");
    }
    Ok(())
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
    /// Minimum time between workspace moves from one bar's wheel input.
    /// This keeps a free-spinning wheel smooth without losing ratchet clicks.
    pub scroll_repeat_ms: u64,
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
            scroll_repeat_ms: 75,
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

/// A popup spatial value expressed either in logical pixels or as a percentage
/// of the host output's corresponding logical dimension.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConfigLength {
    Pixels(f32),
    Percent(f32),
}

impl ConfigLength {
    pub const fn pixels(value: f32) -> Self {
        Self::Pixels(value)
    }

    pub fn resolve(self, total: f32) -> f32 {
        match self {
            Self::Pixels(value) => value,
            Self::Percent(value) => total * value / 100.0,
        }
    }
}

impl Serialize for ConfigLength {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Self::Pixels(value)
                if value.fract() == 0.0
                    && *value >= i64::MIN as f32
                    && *value <= i64::MAX as f32 =>
            {
                serializer.serialize_i64(*value as i64)
            }
            Self::Pixels(value) => serializer.serialize_f32(*value),
            Self::Percent(value) => serializer.serialize_str(&format!("{value}%")),
        }
    }
}

impl<'de> Deserialize<'de> for ConfigLength {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Input {
            Integer(i64),
            Float(f64),
            Text(String),
        }

        match Input::deserialize(deserializer)? {
            Input::Integer(value) => Ok(Self::Pixels(value as f32)),
            Input::Float(value) if value.is_finite() => Ok(Self::Pixels(value as f32)),
            Input::Float(_) => Err(serde::de::Error::custom("length must be finite")),
            Input::Text(value) => {
                let Some(percent) = value.trim().strip_suffix('%') else {
                    return Err(serde::de::Error::custom(
                        "length strings must be percentages such as \"25%\"",
                    ));
                };
                let percent = percent.trim().parse::<f32>().map_err(|_| {
                    serde::de::Error::custom("invalid percentage length; expected e.g. \"25%\"")
                })?;
                if !percent.is_finite() {
                    return Err(serde::de::Error::custom("length must be finite"));
                }
                Ok(Self::Percent(percent))
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PopupConfig {
    pub outputs: PopupOutputs,
    pub surface_mode: PopupSurfaceMode,
    pub follow_delay_ms: u64,
    pub position: PopupPosition,
    pub width: ConfigLength,
    pub height: ConfigLength,
    pub fixed_x: ConfigLength,
    pub fixed_y: ConfigLength,
    /// Per-output anchors, used when `position = "anchored"`. Outputs without an
    /// entry fall back to centred.
    pub placements: Vec<PopupPlacement>,
    /// Size of one monitor's canvas. Each output gets its own canvas and they
    /// are laid out side by side inside the panel.
    pub canvas_width: ConfigLength,
    pub canvas_height: ConfigLength,
    /// Draw workspace and output names. Off by default for the icon-only look.
    pub show_labels: bool,
    pub show_titles: bool,
}

impl Default for PopupConfig {
    fn default() -> Self {
        Self {
            outputs: PopupOutputs::Focused,
            surface_mode: PopupSurfaceMode::FollowActive,
            follow_delay_ms: 180,
            position: PopupPosition::Cursor,
            width: ConfigLength::pixels(920.0),
            height: ConfigLength::pixels(250.0),
            fixed_x: ConfigLength::pixels(80.0),
            fixed_y: ConfigLength::pixels(80.0),
            placements: Vec::new(),
            canvas_width: ConfigLength::pixels(300.0),
            canvas_height: ConfigLength::pixels(190.0),
            show_labels: false,
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
    /// Pin the panel to an edge of whichever monitor hosts it, per
    /// [`PopupConfig::placements`].
    Anchored,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorX {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AnchorY {
    Top,
    #[default]
    Center,
    Bottom,
}

/// Where the panel sits on one particular monitor.
///
/// Each popup surface resolves its placement against its own host output.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PopupPlacement {
    /// Niri connector name, for example `HDMI-A-1`.
    pub output: String,
    pub x: AnchorX,
    pub y: AnchorY,
    /// Gap kept between the panel and the edges it is anchored to.
    /// Percentage values resolve against width for X and height for Y.
    pub margin: ConfigLength,
}

impl Default for PopupPlacement {
    fn default() -> Self {
        Self {
            output: String::new(),
            x: AnchorX::default(),
            y: AnchorY::default(),
            margin: ConfigLength::pixels(48.0),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UrgentIndicator {
    None,
    Ring,
    #[default]
    Glow,
    Badge,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    pub background: StyleColor,
    /// Bottom stop of the popup's vertical background gradient. Set equal to
    /// `background` for a flat panel.
    pub background_end: StyleColor,
    pub surface: StyleColor,
    pub surface_hover: StyleColor,
    pub border: StyleColor,
    pub border_soft: StyleColor,
    pub accent: StyleColor,
    pub selection_border: StyleColor,
    pub text: StyleColor,
    pub muted: StyleColor,
    /// Colour used by urgent window indicators and workspace urgency markers.
    pub urgent: StyleColor,
    /// Preset used to mark urgent window icons.
    pub urgent_indicator: UrgentIndicator,
    /// Deprecated compatibility switch. `urgent_indicator` alone controls
    /// urgency rendering. An explicit `false` combined with the `ring` preset
    /// maps to `urgent_indicator = "none"` during load; every other value is
    /// ignored.
    #[serde(default)]
    pub show_urgent_ring: Option<bool>,
    /// Stroke width of the urgent window ring in logical pixels.
    pub urgent_ring_width: f32,
    /// Gap between an urgent window icon and its ring in logical pixels.
    pub urgent_ring_padding: f32,
    /// Distance that the `glow` preset extends behind an icon.
    pub urgent_glow_spread: f32,
    /// Maximum opacity of the `glow` preset from 0.0 to 1.0.
    pub urgent_glow_alpha: f32,
    /// Radius of the top-right dot used by the `badge` preset.
    pub urgent_badge_radius: f32,
    /// Drop shadow behind the popup panel.
    pub shadow: StyleColor,
    /// Disc drawn behind a floating window's icon so it stays legible.
    pub floating_backing: StyleColor,
    pub radius: f32,
    /// Corner radius of the popup panel. Larger than `radius`, which applies to
    /// smaller elements such as the urgency ring.
    pub panel_radius: f32,
    pub border_width: f32,
    pub icon_size: u32,
    /// Size of the selected icon. Emphasis by scale is what replaces the
    /// selection box.
    pub icon_size_focused: u32,
    /// Opacity applied to icons outside the selected workspace.
    pub dim_alpha: f32,
    pub gap: f32,

    // Primary popup layout controls, so a theme can change the overall
    // proportions without touching the renderer.
    /// Inset between the panel edge and the canvases.
    pub panel_padding: f32,
    /// Space between one monitor's canvas and the next.
    pub canvas_gap: f32,
    /// Added to `icon_size_focused` to get the horizontal step between windows.
    pub column_gap: f32,
    /// Added to `icon_size` to get the vertical step inside a stacked column.
    pub stack_gap: f32,
    /// Added to `icon_size_focused` to get the vertical step between workspaces.
    pub row_gap: f32,
    /// How far the shadow is offset down and out from the panel.
    pub shadow_offset: f32,
    /// Extra spread of the outer, softer shadow pass.
    pub shadow_spread: f32,
    /// Radius of the dot standing in for an empty workspace.
    pub empty_dot_radius: f32,
    /// Font size for workspace and output names.
    pub label_font_size: f32,
    /// Font size for the selected window's title.
    pub title_font_size: f32,
}

impl Default for StyleConfig {
    fn default() -> Self {
        Self {
            background: StyleColor::parse("#f21b2530").expect("built-in colour"),
            background_end: StyleColor::parse("#f20d1117").expect("built-in colour"),
            surface: StyleColor::parse("#661d2022").expect("built-in colour"),
            surface_hover: StyleColor::parse("#266ed2ff").expect("built-in colour"),
            border: StyleColor::parse("#5c8aa5c0").expect("built-in colour"),
            border_soft: StyleColor::parse("#2a8aa5c0").expect("built-in colour"),
            accent: StyleColor::parse("#ff6ed2ff").expect("built-in colour"),
            selection_border: StyleColor::parse("#666ed2ff").expect("built-in colour"),
            text: StyleColor::parse("#ffe1e2e5").expect("built-in colour"),
            muted: StyleColor::parse("#ffc0c7cd").expect("built-in colour"),
            urgent: StyleColor::parse("#ffffb4ab").expect("built-in colour"),
            urgent_indicator: UrgentIndicator::Glow,
            show_urgent_ring: None,
            urgent_ring_width: 1.25,
            urgent_ring_padding: 2.0,
            urgent_glow_spread: 6.0,
            urgent_glow_alpha: 0.18,
            urgent_badge_radius: 3.5,
            shadow: StyleColor::parse("#4e000000").expect("built-in colour"),
            floating_backing: StyleColor::parse("#be0c1016").expect("built-in colour"),
            radius: 14.0,
            panel_radius: 20.0,
            border_width: 1.0,
            icon_size: 28,
            icon_size_focused: 40,
            dim_alpha: 0.45,
            gap: 8.0,
            panel_padding: 14.0,
            canvas_gap: 12.0,
            column_gap: 10.0,
            stack_gap: 6.0,
            row_gap: 16.0,
            shadow_offset: 6.0,
            shadow_spread: 4.0,
            empty_dot_radius: 3.0,
            label_font_size: 11.0,
            title_font_size: 10.0,
        }
    }
}

impl StyleConfig {
    fn validate(&self) -> Result<()> {
        for (field, value) in [
            ("style.urgent_ring_width", self.urgent_ring_width),
            ("style.urgent_ring_padding", self.urgent_ring_padding),
            ("style.urgent_glow_spread", self.urgent_glow_spread),
            ("style.urgent_badge_radius", self.urgent_badge_radius),
            ("style.radius", self.radius),
            ("style.panel_radius", self.panel_radius),
            ("style.border_width", self.border_width),
            ("style.gap", self.gap),
            ("style.panel_padding", self.panel_padding),
            ("style.canvas_gap", self.canvas_gap),
            ("style.column_gap", self.column_gap),
            ("style.stack_gap", self.stack_gap),
            ("style.row_gap", self.row_gap),
            ("style.shadow_spread", self.shadow_spread),
            ("style.empty_dot_radius", self.empty_dot_radius),
        ] {
            validate_f32_range(field, value, 0.0, MAX_STYLE_LENGTH)?;
        }
        validate_f32_range(
            "style.shadow_offset",
            self.shadow_offset,
            -MAX_STYLE_LENGTH,
            MAX_STYLE_LENGTH,
        )?;
        validate_f32_range("style.dim_alpha", self.dim_alpha, 0.0, 1.0)?;
        validate_f32_range("style.urgent_glow_alpha", self.urgent_glow_alpha, 0.0, 1.0)?;
        validate_f32_range(
            "style.label_font_size",
            self.label_font_size,
            1.0,
            MAX_FONT_SIZE,
        )?;
        validate_f32_range(
            "style.title_font_size",
            self.title_font_size,
            1.0,
            MAX_FONT_SIZE,
        )?;
        validate_u32_range("style.icon_size", self.icon_size, 1, MAX_ICON_SIZE)?;
        validate_u32_range(
            "style.icon_size_focused",
            self.icon_size_focused,
            1,
            MAX_ICON_SIZE,
        )?;
        Ok(())
    }
}

/// Timing and easing of the popup's transitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimationConfig {
    /// With this off every transition is applied instantly.
    pub enabled: bool,
    /// Panel open and close, in milliseconds.
    pub panel_ms: f32,
    /// Selection and hover emphasis, in milliseconds.
    pub emphasis_ms: f32,
    /// Canvas scrolling under the focal point, in milliseconds.
    pub scroll_ms: f32,
    /// Scale the panel grows from when opening. 1.0 disables the scale.
    pub open_scale: f32,
}

impl Default for AnimationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            panel_ms: 140.0,
            emphasis_ms: 180.0,
            scroll_ms: 165.0,
            open_scale: 0.94,
        }
    }
}

/// Passive overlay shown while navigating Niri with its own keybinds.
///
/// The popup appears by itself, mirrors where focus has moved to, and hides
/// again once navigation stops. It never takes pointer or keyboard input, so
/// the keybinds driving the navigation keep working.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PeekConfig {
    pub enabled: bool,
    /// How long the overlay lingers after the last focus change. Each further
    /// change restarts the countdown, so it stays up while you keep moving.
    pub duration_ms: u64,
    /// Let the pointer take the overlay over instead of it staying click-through.
    ///
    /// The overlay accepts pointer input over the panel rectangle only, and only
    /// while it is on screen. Moving onto it holds it open; clicking it turns it
    /// into a full manual session in place. The keyboard is never taken, so the
    /// keybinds that raised the overlay keep working while the pointer rests on
    /// it.
    ///
    /// Off by default: it trades a click-through panel for a grabbable one, and
    /// that is a change of character rather than a refinement.
    pub pointer_promote: bool,
    /// Allow moving an armed peek from its border. Off keeps peek passive.
    pub move_popup: bool,
    /// How long the overlay stays click-through after appearing.
    ///
    /// A click already travelling towards whatever the overlay just covered must
    /// still land there, so the pointer region is withheld for this long.
    pub arm_grace_ms: u64,
    /// How long the overlay lingers after the pointer leaves it again.
    ///
    /// Shorter than `duration_ms`: the user just declined it by moving away, and
    /// the full countdown would read as the panel refusing to go.
    pub demote_ms: u64,
}

impl Default for PeekConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 1200,
            pointer_promote: false,
            move_popup: false,
            arm_grace_ms: 150,
            demote_ms: 400,
        }
    }
}

/// How keyboard navigation in the popup relates to Niri's real focus.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NavigationMode {
    /// Move a selection inside the popup and apply it only on confirm.
    Deferred,
    /// Move Niri's focus with every step, alt-tab style.
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    /// `deferred` queues the move until confirmed; `live` follows immediately.
    pub navigation: NavigationMode,
    pub drag_threshold: f64,
    /// Allow a manual popup to move when its border is pressed and held.
    pub move_popup: bool,
    /// Border inset, in logical pixels, that starts a popup move press.
    pub popup_grab_region: f64,
    pub empty_drop_floats: bool,
    pub floating_drop_tiles: bool,
    pub focus_after_drop: bool,
    pub snap_cursor_after_drop: bool,
    pub close_on_window_click: bool,
    pub close_on_workspace_click: bool,
    pub close_on_drop: bool,
    /// Close when Niri focus changes outside an XMB action.
    pub close_on_external_focus_change: bool,
    pub close_on_outside_click: bool,
    /// Replay a captured outside click after the popup closes.
    pub replay_outside_click: bool,
    pub close_on_escape: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            navigation: NavigationMode::Deferred,
            drag_threshold: 7.0,
            move_popup: false,
            popup_grab_region: 24.0,
            empty_drop_floats: false,
            floating_drop_tiles: true,
            focus_after_drop: false,
            snap_cursor_after_drop: false,
            close_on_window_click: true,
            close_on_workspace_click: false,
            close_on_drop: false,
            close_on_external_focus_change: false,
            close_on_outside_click: true,
            replay_outside_click: true,
            close_on_escape: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load_source(config_source: &str, theme_source: Option<&str>) -> Result<Config> {
        let dir = std::env::temp_dir().join(format!(
            "niri-xmb-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, config_source).unwrap();
        if let Some(theme_source) = theme_source {
            std::fs::write(dir.join("theme.toml"), theme_source).unwrap();
        }
        let result = Config::load(&config_path);
        std::fs::remove_dir_all(&dir).ok();
        result
    }

    #[test]
    fn default_config_round_trips() {
        let encoded = toml::to_string_pretty(&Config::default()).unwrap();
        toml::from_str::<Config>(&encoded).unwrap();
    }

    #[test]
    fn shipped_example_config_parses() {
        let source = include_str!("../../../config/niri-xmb.toml.example");
        let config = toml::from_str::<Config>(source).unwrap();
        config.validate().unwrap();
        // The example must not name this developer's connectors: a copied
        // config has to auto-detect bars on whatever monitors the user has.
        assert!(config.bars_enabled);
        assert!(
            config.bars.is_empty(),
            "shipped example must not pin [[bars]] to specific connectors"
        );
    }

    #[test]
    fn popup_lengths_accept_pixels_and_percentages() {
        let config = toml::from_str::<Config>(
            r#"
            [popup]
            width = "80%"
            height = 720
            fixed_x = "12.5%"
            fixed_y = -24
            canvas_width = "18%"
            canvas_height = 190.5

            [[popup.placements]]
            output = "HDMI-A-1"
            margin = "3%"
            "#,
        )
        .unwrap();

        assert_eq!(config.popup.width.resolve(1000.0), 800.0);
        assert_eq!(config.popup.height.resolve(1000.0), 720.0);
        assert_eq!(config.popup.fixed_x.resolve(800.0), 100.0);
        assert_eq!(config.popup.fixed_y.resolve(800.0), -24.0);
        assert_eq!(config.popup.canvas_width.resolve(1000.0), 180.0);
        assert_eq!(config.popup.canvas_height.resolve(1000.0), 190.5);
        assert_eq!(config.popup.placements[0].margin.resolve(1000.0), 30.0);
    }

    #[test]
    fn urgent_window_ring_is_configurable() {
        let config = toml::from_str::<Config>(
            r##"
            [style]
            urgent = "#ff112233"
            show_urgent_ring = false
            urgent_ring_width = 3.5
            urgent_ring_padding = 6.0
            "##,
        )
        .unwrap();

        assert_eq!(config.style.urgent.to_string(), "#FF112233");
        assert_eq!(config.style.urgent_ring_width, 3.5);
        assert_eq!(config.style.urgent_ring_padding, 6.0);
    }

    #[test]
    fn legacy_urgent_ring_switch_maps_to_none() {
        // The deprecated switch suppressed the ring preset; that combination
        // keeps its meaning instead of starting to draw a ring.
        let mut config = toml::from_str::<Config>(
            r#"
            [style]
            urgent_indicator = "ring"
            show_urgent_ring = false
            "#,
        )
        .unwrap();
        config.reconcile_legacy_switches();
        assert_eq!(config.style.urgent_indicator, UrgentIndicator::None);

        // An explicit false with any other preset changes nothing, and the
        // switch is consumed either way.
        let mut config = toml::from_str::<Config>(
            r#"
            [style]
            urgent_indicator = "glow"
            show_urgent_ring = false
            "#,
        )
        .unwrap();
        config.reconcile_legacy_switches();
        assert_eq!(config.style.urgent_indicator, UrgentIndicator::Glow);
        assert_eq!(config.style.show_urgent_ring, None);

        // Omitting the switch leaves the ring preset active.
        let mut config =
            toml::from_str::<Config>("[style]\nurgent_indicator = \"ring\"\n").unwrap();
        config.reconcile_legacy_switches();
        assert_eq!(config.style.urgent_indicator, UrgentIndicator::Ring);
    }

    #[test]
    fn urgent_indicator_presets_parse() {
        for (name, expected) in [
            ("none", UrgentIndicator::None),
            ("ring", UrgentIndicator::Ring),
            ("glow", UrgentIndicator::Glow),
            ("badge", UrgentIndicator::Badge),
        ] {
            let config =
                toml::from_str::<Config>(&format!("[style]\nurgent_indicator = \"{name}\"\n"))
                    .unwrap();
            assert_eq!(config.style.urgent_indicator, expected);
        }
    }

    #[test]
    fn invalid_popup_percentage_is_rejected() {
        let error = toml::from_str::<Config>(
            r#"
            [popup]
            width = "mostly"
            "#,
        )
        .expect_err("non-percentage length string should fail");

        assert!(error.to_string().contains("percentage"));
    }

    #[test]
    fn semantic_validation_rejects_unsafe_dimensions_and_invalid_colors() {
        let config = toml::from_str::<Config>(
            r##"
            [[bars]]
            output = "eDP-1"
            width = 9000
            "##,
        )
        .unwrap();

        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("bars[0].width"), "unexpected error: {error}");

        let config = toml::from_str::<Config>(
            r#"
            [[bars]]
            output = "eDP-1"
            edge = "top"
            height = 513
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("bars[0].height"),
            "unexpected error: {error}"
        );

        let config = toml::from_str::<Config>(
            r#"
            [[bars]]
            output = "eDP-1"
            edge = "left"
            width = 513
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("bars[0].width"), "unexpected error: {error}");
    }

    #[test]
    fn semantic_validation_rejects_duplicate_outputs_and_placements() {
        let config = toml::from_str::<Config>(
            r#"
            [[bars]]
            output = "eDP-1"

            [[bars]]
            output = "eDP-1"
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("duplicates output"),
            "unexpected error: {error}"
        );

        let config = toml::from_str::<Config>(
            r#"
            [[popup.placements]]
            output = "eDP-1"

            [[popup.placements]]
            output = "eDP-1"
            "#,
        )
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("popup.placements[1].output duplicates"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn semantic_validation_rejects_nonfinite_and_out_of_range_values() {
        let mut config = Config::default();
        config.animation.panel_ms = f32::NAN;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("animation.panel_ms"));

        config = Config::default();
        config.popup.width = ConfigLength::Percent(101.0);
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("popup.width"));

        config = Config::default();
        config.popup.fixed_x = ConfigLength::Percent(-25.0);
        config.validate().unwrap();
    }

    #[test]
    fn merge_style_overrides_only_theme_keys() {
        let mut base = toml::Value::try_from(StyleConfig::default()).unwrap();
        let theme: toml::Value = toml::from_str(
            r##"
            accent = "#ff112233"
            text = "#ff445566"
            "##,
        )
        .unwrap();

        merge_style(&mut base, &theme);

        let base_table = base.as_table().unwrap();
        assert_eq!(
            base_table.get("accent").unwrap().as_str().unwrap(),
            "#ff112233"
        );
        assert_eq!(
            base_table.get("text").unwrap().as_str().unwrap(),
            "#ff445566"
        );
        // Untouched fields keep the base's values.
        let defaults = StyleConfig::default();
        assert!(
            base_table
                .get("background")
                .unwrap()
                .as_str()
                .unwrap()
                .eq_ignore_ascii_case(&defaults.background.to_string()),
        );
        assert_eq!(
            base_table.get("radius").unwrap().as_float().unwrap(),
            defaults.radius as f64
        );
    }

    #[test]
    fn absent_theme_field_behaves_as_before() {
        let source = include_str!("../../../config/niri-xmb.toml.example");
        let config = toml::from_str::<Config>(source).unwrap();
        assert!(config.theme.is_none());
    }

    #[test]
    fn resolve_theme_path_handles_tilde_and_relative() {
        let config_dir = Path::new("/home/user/.config/niri-xmb");

        let relative = resolve_theme_path("themes/dank.toml", config_dir);
        assert_eq!(relative, config_dir.join("themes/dank.toml"));

        let absolute = resolve_theme_path("/etc/niri-xmb/theme.toml", config_dir);
        assert_eq!(absolute, PathBuf::from("/etc/niri-xmb/theme.toml"));

        if let Some(home) = std::env::var_os("HOME") {
            let tilde = resolve_theme_path("~/themes/dank.toml", config_dir);
            assert_eq!(tilde, PathBuf::from(home).join("themes/dank.toml"));
        }
    }

    #[test]
    fn missing_theme_file_errors() {
        let dir = std::env::temp_dir().join(format!(
            "niri-xmb-config-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(&config_path, "theme = \"does-not-exist.toml\"\n").unwrap();

        let result = Config::load(&config_path);

        std::fs::remove_dir_all(&dir).ok();

        let err = result.expect_err("missing theme file should error");
        let message = format!("{err:#}");
        assert!(
            message.contains("failed to read theme"),
            "unexpected error message: {message}"
        );
    }

    #[test]
    fn referenced_theme_requires_a_style_table() {
        let error =
            load_source("theme = \"theme.toml\"\n", Some("accent = \"#ff112233\"\n")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("missing required [style]"), "{message}");
    }

    #[test]
    fn referenced_theme_rejects_wrong_style_type_and_unknown_keys() {
        let error =
            load_source("theme = \"theme.toml\"\n", Some("style = \"dark\"\n")).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("style must be a table"), "{message}");

        let error = load_source(
            "theme = \"theme.toml\"\n",
            Some("[style]\naccent = \"#ff112233\"\naccentt = \"#ff112233\"\n"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("unknown style key style.accentt"),
            "{message}"
        );
    }

    #[test]
    fn referenced_theme_rejects_invalid_color_and_top_level_keys() {
        let error = load_source(
            "theme = \"theme.toml\"\n",
            Some("[style]\naccent = \"blue\"\n"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("invalid") || message.contains("style"),
            "{message}"
        );

        let error = load_source(
            "theme = \"theme.toml\"\n",
            Some("name = \"test\"\n[style]\naccent = \"#ff112233\"\n"),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("unsupported top-level key"), "{message}");
    }

    #[test]
    fn config_load_runs_semantic_validation() {
        let error = load_source(
            r#"
            [[bars]]
            output = "eDP-1"
            height = 0
            "#,
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("bars[0].height"), "{message}");
    }

    #[test]
    fn shipped_themes_parse_and_merge() {
        let dank = include_str!("../../../themes/dank.toml");
        let noctalia = include_str!("../../../themes/noctalia.toml");
        let material = include_str!("../../../themes/material-3-expressive.toml");
        let liquid_glass = include_str!("../../../themes/liquid-glass.toml");

        for theme_source in [dank, noctalia, material, liquid_glass] {
            let mut base = toml::Value::try_from(StyleConfig::default()).unwrap();
            let theme_value: toml::Value = toml::from_str(theme_source).unwrap();
            let theme_style = theme_value.get("style").expect("theme has [style] table");
            merge_style(&mut base, theme_style);

            let merged: StyleConfig = base.try_into().unwrap();
            merged.validate().unwrap();
            // A couple of layout fields must survive untouched since the
            // shipped themes only override colours.
            let defaults = StyleConfig::default();
            assert_eq!(merged.radius, defaults.radius);
            assert_eq!(merged.icon_size, defaults.icon_size);
            assert_eq!(merged.panel_padding, defaults.panel_padding);

            // And the theme's own colours must have taken effect.
            let theme_table = theme_style.as_table().unwrap();
            for (key, value) in theme_table {
                let expected = value.as_str().unwrap();
                let actual = match key.as_str() {
                    "background" => &merged.background,
                    "background_end" => &merged.background_end,
                    "surface" => &merged.surface,
                    "surface_hover" => &merged.surface_hover,
                    "border" => &merged.border,
                    "border_soft" => &merged.border_soft,
                    "accent" => &merged.accent,
                    "selection_border" => &merged.selection_border,
                    "text" => &merged.text,
                    "muted" => &merged.muted,
                    "urgent" => &merged.urgent,
                    "shadow" => &merged.shadow,
                    "floating_backing" => &merged.floating_backing,
                    other => panic!("unexpected style key in shipped theme: {other}"),
                };
                assert_eq!(actual.to_string().to_uppercase(), expected.to_uppercase());
            }
        }
    }

    /// Colours are parsed once at load, so an invalid one is refused there
    /// rather than silently becoming black at render time.
    #[test]
    fn invalid_colours_are_refused_at_load() {
        assert!(toml::from_str::<Config>("[style]\naccent = \"1e1e2e\"\n").is_err());
        assert!(toml::from_str::<Config>("[style]\naccent = \"#12345\"\n").is_err());
        assert!(toml::from_str::<Config>("[style]\naccent = \"#gggggg\"\n").is_err());
        assert!(toml::from_str::<Config>("[style]\naccent = \"#1e1e2e\"\n").is_ok());
        assert!(toml::from_str::<Config>("[style]\naccent = \"#ff1e1e2e\"\n").is_ok());
    }
}
