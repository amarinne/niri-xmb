use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

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
            return Ok(Self::default());
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
            let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
            let resolved = resolve_theme_path(&theme, config_dir);
            let theme_source = std::fs::read_to_string(&resolved)
                .with_context(|| format!("failed to read theme {}", resolved.display()))?;
            let theme_value: toml::Value = toml::from_str(&theme_source)
                .with_context(|| format!("failed to parse theme {}", resolved.display()))?;

            let table = value
                .as_table_mut()
                .expect("top-level config is always a table");
            let style = table
                .entry("style")
                .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
            if let Some(theme_style) = theme_value.get("style") {
                merge_style(style, theme_style);
            }
        }

        value
            .try_into()
            .with_context(|| format!("failed to parse {}", path.display()))
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StyleConfig {
    pub background: String,
    /// Bottom stop of the popup's vertical background gradient. Set equal to
    /// `background` for a flat panel.
    pub background_end: String,
    pub surface: String,
    pub surface_hover: String,
    pub border: String,
    pub border_soft: String,
    pub accent: String,
    pub selection_border: String,
    pub text: String,
    pub muted: String,
    pub urgent: String,
    /// Drop shadow behind the popup panel.
    pub shadow: String,
    /// Disc drawn behind a floating window's icon so it stays legible.
    pub floating_backing: String,
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
            background: "#f21b2530".into(),
            background_end: "#f20d1117".into(),
            surface: "#661d2022".into(),
            surface_hover: "#266ed2ff".into(),
            border: "#5c8aa5c0".into(),
            border_soft: "#2a8aa5c0".into(),
            accent: "#ff6ed2ff".into(),
            selection_border: "#666ed2ff".into(),
            text: "#ffe1e2e5".into(),
            muted: "#ffc0c7cd".into(),
            urgent: "#ffffb4ab".into(),
            shadow: "#4e000000".into(),
            floating_backing: "#be0c1016".into(),
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
    /// Also peek when focus moves between windows, not only between
    /// workspaces.
    ///
    /// Off by default. Window focus moves constantly for reasons that are not
    /// navigation — a dialog closing hands focus back, and focus-follows-mouse
    /// retargets on every pointer cross — and an overlay that reacts to all of
    /// it reads as noise rather than as a workspace navigator.
    pub on_window_focus: bool,
}

impl Default for PeekConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            duration_ms: 1200,
            on_window_focus: false,
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
    pub empty_drop_floats: bool,
    pub floating_drop_tiles: bool,
    pub focus_after_drop: bool,
    pub snap_cursor_after_drop: bool,
    pub close_on_window_click: bool,
    pub close_on_workspace_click: bool,
    pub close_on_drop: bool,
    pub close_on_outside_click: bool,
    pub close_on_escape: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            navigation: NavigationMode::Deferred,
            drag_threshold: 7.0,
            empty_drop_floats: false,
            floating_drop_tiles: true,
            focus_after_drop: false,
            snap_cursor_after_drop: false,
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
        assert_eq!(
            base_table.get("background").unwrap().as_str().unwrap(),
            defaults.background
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
    fn shipped_themes_parse_and_merge() {
        let dank = include_str!("../../../themes/dank.toml");
        let noctalia = include_str!("../../../themes/noctalia.toml");

        for theme_source in [dank, noctalia] {
            let mut base = toml::Value::try_from(StyleConfig::default()).unwrap();
            let theme_value: toml::Value = toml::from_str(theme_source).unwrap();
            let theme_style = theme_value.get("style").expect("theme has [style] table");
            merge_style(&mut base, theme_style);

            let merged: StyleConfig = base.try_into().unwrap();
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
                assert_eq!(actual, expected);
            }
        }
    }
}
