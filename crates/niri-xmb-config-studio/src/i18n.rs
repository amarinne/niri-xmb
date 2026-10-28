//! Display-text catalog and translation template.
//!
//! egui has no built-in internationalisation. The Rust ecosystem offers
//! Mozilla Fluent (via `i18n-embed` / `fluent-templates`, `.ftl` files) and
//! gettext (`.po` files); both add a runtime and a dependency this app does
//! not need yet, because no translation exists. This module is the
//! convention until one does:
//!
//! * Every user-visible string has a stable key here. Field keys are the
//!   config path itself (`gamepad.navi.pointer.deadzone`); everything else
//!   lives under `ui.` (`ui.action.save`, `ui.card.pointer`, …).
//! * [`CATALOG`] is the English source of truth. The field helpers resolve
//!   their displayed label and hover text from it by config path, so the
//!   text on screen and the text in the template cannot drift apart.
//! * [`en_template_json`] renders the catalog as `locales/en.json`, in the
//!   same `{ label, description }` shape as the Noctalia plugin's
//!   `translations/en.json` in this repo. A test keeps the file in sync;
//!   regenerate it with `UPDATE_LOCALES=1 cargo test -p
//!   niri-xmb-config-studio i18n` or `--print-strings`.
//! * To translate later: copy `locales/en.json` to `locales/<lang>.json`,
//!   translate the values, keep the keys. A runtime loader then only has
//!   to look keys up in that file with English fallback — or the same
//!   stable keys can be moved into Fluent `.ftl` files mechanically.
//!
//! `description` is the hover text shown in the UI and the translator's
//! context note. Keep it one sentence: what it does, its unit or range,
//! and what a higher or lower value does, where that is not obvious.

use serde_json::{Map, Value, json};

/// One display string: stable key, English label, hover/context text.
pub struct Entry {
    pub key: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

macro_rules! e {
    ($key:expr, $label:expr, $desc:expr) => {
        Entry {
            key: $key,
            label: $label,
            description: $desc,
        }
    };
}

/// Every catalogued display string. Field entries come first, keyed by
/// config path (dynamic segments written `*`), then `ui.*` entries.
pub static CATALOG: &[Entry] = &[
    // -- Controller: device -------------------------------------------------
    e!(
        "gamepad.enabled",
        "Enabled",
        "Master switch for gamepad input. Off: the daemon ignores controllers entirely."
    ),
    e!(
        "gamepad.preset",
        "Preset",
        "Built-in button layout: none (only your explicit bindings), auto (detect Xbox/DS4/Flydigi per device), xbox, or ds4."
    ),
    e!(
        "gamepad.devices",
        "Devices",
        "evdev device paths to read, comma-separated. Empty: auto-discover /dev/input/by-id/*-event-joystick and *-event-gamepad."
    ),
    e!(
        "gamepad.open_combo",
        "Open combo",
        "Buttons held together (at least two) to open the XMB popup. Empty disables the combo."
    ),
    e!(
        "gamepad.mode.default",
        "Default mode",
        "Mode at startup: game (input passes through to games) or navi (controller drives the desktop)."
    ),
    e!(
        "gamepad.mode.toggle",
        "Mode toggle",
        "Buttons held together (at least two) to switch between game and navi mode. Empty disables the toggle."
    ),
    e!(
        "gamepad.mode.grab_in_navi",
        "Grab in navi",
        "Grab the controller exclusively in navi mode so games do not also receive the input."
    ),
    e!(
        "gamepad.mode.reset_on_disconnect",
        "Reset on disconnect",
        "Return to game mode when the last controller disconnects."
    ),
    // -- Controller: pointer (motion) ---------------------------------------
    e!(
        "gamepad.navi.pointer.enabled",
        "Enabled",
        "Use an analog stick to move the desktop pointer in navi mode."
    ),
    e!(
        "gamepad.navi.pointer.stick",
        "Stick",
        "Which analog stick moves the pointer, left or right. The other stick can scroll."
    ),
    e!(
        "gamepad.navi.pointer.deadzone",
        "Deadzone",
        "Stick tilt below this fraction (0–1) is ignored, so a resting or drifting stick does not move the pointer. Higher needs a bigger push to start."
    ),
    e!(
        "gamepad.navi.pointer.curve",
        "Curve",
        "Pointer response exponent applied after the deadzone. 1.0 is linear; higher keeps small tilts slower for precision and reaches full speed only near full tilt; lower reacts faster early."
    ),
    e!(
        "gamepad.navi.pointer.speed",
        "Speed",
        "Pointer speed in logical pixels per second at full stick deflection, before trigger speed modulation."
    ),
    e!(
        "gamepad.navi.pointer.max_step",
        "Max step",
        "Largest pointer movement emitted in a single frame, in logical pixels. Caps bursts and lag spikes; 0 disables the cap."
    ),
    e!(
        "gamepad.navi.pointer.invert_y",
        "Invert Y",
        "Flip vertical pointer movement: push the stick up to move the pointer down, and vice versa."
    ),
    // -- Controller: pointer (scroll) ---------------------------------------
    e!(
        "gamepad.navi.pointer.scroll_enabled",
        "Scroll enabled",
        "Enable scrolling via the analog scroll stick or scroll buttons in navi mode."
    ),
    e!(
        "gamepad.navi.pointer.scroll_invert_y",
        "Invert Y",
        "Invert scroll direction: push the stick up to scroll down, and vice versa."
    ),
    e!(
        "gamepad.navi.pointer.scroll_stick",
        "Scroll stick",
        "Which analog stick scrolls, left or right. Pick the stick not used for pointer motion."
    ),
    e!(
        "gamepad.navi.pointer.scroll_deadzone",
        "Scroll deadzone",
        "Stick tilt below this fraction (0–1) is ignored for scrolling, so a resting stick does not scroll. Higher needs a bigger push to start."
    ),
    e!(
        "gamepad.navi.pointer.scroll_speed",
        "Full-stick speed",
        "Scroll rate in wheel steps per second at full stick deflection."
    ),
    // -- Trigger Lab ----------------------------------------------------------
    e!(
        "gamepad.navi.pointer.trigger_deadzone",
        "Pull deadzone",
        "Initial analog-trigger travel ignored before LT/RT speed modulation starts, as a fraction (0–1) of full pull."
    ),
    e!(
        "gamepad.navi.pointer.trigger_curve",
        "Shape curve",
        "Trigger response exponent when no custom points are set. 1.0 is linear; higher stays slower until a deeper pull."
    ),
    e!(
        "gamepad.navi.pointer.speed_socd",
        "Speed SOCD",
        "How simultaneous LT brake and RT gas resolve (SOCD): last/first pressed wins, both cancel, both multiply, or brake/gas wins. Affects speed only, not bindings or layers."
    ),
    e!(
        "gamepad.navi.pointer.trigger_threshold",
        "Layer threshold",
        "Analog axis pull (0–1) at which a layer hold_axes entry counts as held."
    ),
    e!(
        "gamepad.navi.pointer.accel.enabled",
        "Enabled",
        "Let the triggers modulate pointer speed: LT brakes, RT accelerates."
    ),
    e!(
        "gamepad.navi.pointer.accel.decel_floor",
        "Full brake ×",
        "Pointer speed multiplier at full LT brake. 0.30× means 30% of baseline speed; must be above 0 and at most 1."
    ),
    e!(
        "gamepad.navi.pointer.accel.boost_cap",
        "Full gas ×",
        "Pointer speed multiplier at full RT gas. 2.5× means 250% of baseline speed; between 1 and 10."
    ),
    e!(
        "gamepad.navi.pointer.accel.smoothing_ms",
        "Smoothing ms",
        "How fast the pointer speed multiplier eases toward its target, in milliseconds. Higher is smoother and laggier; 0 is instant."
    ),
    e!(
        "gamepad.navi.pointer.scroll_accel.enabled",
        "Enabled",
        "Let the triggers modulate scroll speed: LT brakes, RT accelerates."
    ),
    e!(
        "gamepad.navi.pointer.scroll_accel.decel_floor",
        "Full brake ×",
        "Scroll speed multiplier at full LT brake. 0.35× means 35% of baseline speed; must be above 0 and at most 1."
    ),
    e!(
        "gamepad.navi.pointer.scroll_accel.boost_cap",
        "Full gas ×",
        "Scroll speed multiplier at full RT gas. 3.0× means 300% of baseline speed; between 1 and 10."
    ),
    e!(
        "gamepad.navi.pointer.scroll_accel.smoothing_ms",
        "Smoothing ms",
        "How fast the scroll speed multiplier eases toward its target, in milliseconds. Higher is smoother and laggier; 0 is instant."
    ),
    // -- Popup: behavior ------------------------------------------------------
    e!(
        "behavior.navigation",
        "Navigation",
        "deferred: move a selection in the popup and apply it only on confirm. live: move Niri focus with every step, alt-tab style."
    ),
    e!(
        "behavior.move_popup",
        "Border drag",
        "Allow moving the manual popup by pressing and holding its border."
    ),
    e!(
        "behavior.popup_grab_region",
        "Grab region",
        "Border inset in logical pixels that starts a popup-move press when Border drag is on."
    ),
    e!(
        "behavior.drag_threshold",
        "Drag threshold",
        "Pointer travel in logical pixels before a press counts as a drag instead of a click."
    ),
    e!(
        "behavior.close_on_outside_click",
        "Outside click",
        "Close the manual popup when a click lands outside it. When off, outside clicks pass through without closing."
    ),
    e!(
        "behavior.replay_outside_click",
        "Replay outside",
        "Replay a captured outside click to what is underneath after the popup closes, so the click still lands."
    ),
    e!(
        "behavior.close_on_window_click",
        "Window click",
        "Close the popup when a window inside it is clicked."
    ),
    e!(
        "behavior.close_on_workspace_click",
        "Workspace click",
        "Close the popup when a workspace inside it is clicked."
    ),
    e!(
        "behavior.close_on_drop",
        "Drop",
        "Close the popup after a drag-and-drop completes."
    ),
    e!(
        "behavior.close_on_external_focus_change",
        "External focus",
        "Close the popup when Niri focus changes for a reason outside the popup itself."
    ),
    e!(
        "behavior.close_on_escape",
        "Escape",
        "Close the popup with Escape (and the gamepad close action)."
    ),
    e!(
        "behavior.empty_drop_floats",
        "Empty drop floats",
        "Dropping a window onto empty popup space makes it floating instead of tiled."
    ),
    e!(
        "behavior.floating_drop_tiles",
        "Floating drop tiles",
        "Dropping a floating window onto the popup tiles it instead of keeping it floating."
    ),
    e!(
        "behavior.focus_after_drop",
        "Focus after drop",
        "Focus the dropped window after a drop completes."
    ),
    e!(
        "behavior.snap_cursor_after_drop",
        "Snap cursor after drop",
        "Move the pointer onto the dropped window after a drop, so it stays under the cursor."
    ),
    // -- Popup: panel ---------------------------------------------------------
    e!(
        "popup.outputs",
        "Outputs",
        "Which monitors the popup covers: focused only, all separately, or all combined into one canvas."
    ),
    e!(
        "popup.surface_mode",
        "Surface",
        "fixed: one surface. follow_active: the surface follows the active output. mirror: mirrored surfaces per output."
    ),
    e!(
        "popup.position",
        "Position",
        "Where the popup appears: at the cursor, at fixed coordinates, centred, or anchored to an edge placement."
    ),
    e!(
        "popup.width",
        "Width",
        "Popup width in logical pixels, or a percentage such as 80%."
    ),
    e!(
        "popup.height",
        "Height",
        "Popup height in logical pixels, or a percentage such as 80%."
    ),
    e!(
        "popup.follow_delay_ms",
        "Follow delay ms",
        "Delay in milliseconds before a follow_active surface moves to the newly active output."
    ),
    e!(
        "popup.fixed_x",
        "Fixed X",
        "Horizontal position in logical pixels when Position is fixed."
    ),
    e!(
        "popup.fixed_y",
        "Fixed Y",
        "Vertical position in logical pixels when Position is fixed."
    ),
    e!(
        "popup.canvas_width",
        "Canvas width",
        "Width of the combined canvas in logical pixels when Outputs is combined."
    ),
    e!(
        "popup.canvas_height",
        "Canvas height",
        "Height of the combined canvas in logical pixels when Outputs is combined."
    ),
    e!(
        "popup.show_labels",
        "Labels",
        "Overlay the output name in the corner of each monitor canvas."
    ),
    e!(
        "popup.show_titles",
        "Titles",
        "Show window titles in the popup."
    ),
    e!(
        "popup.placements.*.output",
        "Output",
        "Niri output name this placement applies to, e.g. DP-1. Empty matches any output."
    ),
    e!(
        "popup.placements.*.x",
        "X",
        "Horizontal anchoring for this placement: left, center, or right."
    ),
    e!(
        "popup.placements.*.y",
        "Y",
        "Vertical anchoring for this placement: top, center, or bottom."
    ),
    e!(
        "popup.placements.*.margin",
        "Margin",
        "Distance in logical pixels from the anchored edge."
    ),
    // -- Popup: peek ------------------------------------------------------------
    e!(
        "peek.enabled",
        "Enabled",
        "Show the passive peek overlay while navigating with Niri keybinds. It mirrors focus, takes no input, and hides when navigation stops."
    ),
    e!(
        "peek.duration_ms",
        "Duration ms",
        "How long peek lingers after the last focus change, in milliseconds. Each further change restarts the countdown."
    ),
    e!(
        "peek.pointer_promote",
        "Pointer promote",
        "Let the pointer take peek over: moving onto it holds it open and clicking turns it into a full manual session. Off keeps peek click-through."
    ),
    e!(
        "peek.move_popup",
        "Border move",
        "Allow moving an armed peek by its border. Off keeps peek passive."
    ),
    e!(
        "peek.arm_grace_ms",
        "Arm grace ms",
        "How long peek stays click-through after appearing, in milliseconds, so a click already travelling underneath still lands there."
    ),
    e!(
        "peek.demote_ms",
        "Demote ms",
        "How long peek lingers after the pointer leaves it, in milliseconds. Shorter than Duration on purpose: moving away means you declined it."
    ),
    // -- Popup: animation ---------------------------------------------------------
    e!(
        "animation.enabled",
        "Enabled",
        "Animate popup open, close, and movement."
    ),
    e!(
        "animation.panel_ms",
        "Panel ms",
        "Panel open/close animation duration in milliseconds."
    ),
    e!(
        "animation.emphasis_ms",
        "Emphasis ms",
        "Hover/selection emphasis animation duration in milliseconds."
    ),
    e!(
        "animation.scroll_ms",
        "Scroll ms",
        "Popup scroll animation duration in milliseconds."
    ),
    e!(
        "animation.open_scale",
        "Open scale",
        "Scale the popup starts from when opening, as a fraction of full size. 0.94 starts at 94%; 1.0 disables the zoom."
    ),
    // -- Popup appearance (theme, colours, geometry, bars) --------------------------
    e!(
        "theme",
        "Theme file",
        "Path to a theme TOML merged over the built-in style. Editing style fields below writes the base config value."
    ),
    e!(
        "style.urgent_indicator",
        "Urgent indicator",
        "How a workspace demanding attention is marked: none, a ring, a glow, or a badge."
    ),
    e!(
        "style.icon_size",
        "Icon size",
        "Window icon size in logical pixels for unfocused workspaces."
    ),
    e!(
        "style.icon_size_focused",
        "Focused icon size",
        "Window icon size in logical pixels for the focused workspace."
    ),
    e!(
        "bars_enabled",
        "Enabled",
        "Master switch for the built-in workspace bars. The Noctalia dock plugin is separate and unaffected."
    ),
    // -- Bars (dynamic index written *) ---------------------------------------------
    e!(
        "bars.*.output",
        "Output",
        "Niri output name this bar sits on, e.g. DP-1. Empty uses the focused output."
    ),
    e!(
        "bars.*.edge",
        "Edge",
        "Screen edge the bar attaches to: top, bottom, left, or right."
    ),
    e!(
        "bars.*.align",
        "Align",
        "Alignment of the bar contents along its edge: start, center, or end."
    ),
    e!("bars.*.width", "Width", "Bar width in logical pixels."),
    e!("bars.*.height", "Height", "Bar height in logical pixels."),
    e!(
        "bars.*.margin",
        "Margin",
        "Distance in logical pixels from the screen edge."
    ),
    e!(
        "bars.*.offset",
        "Offset",
        "Shift along the edge in logical pixels, after alignment and margin."
    ),
    e!(
        "bars.*.allow_column_join",
        "Column join",
        "Treat drops nearest a window as joining its column instead of inserting left or right."
    ),
    e!(
        "bars.*.show_workspace_labels",
        "Workspace labels",
        "Show workspace name labels in the bar."
    ),
    e!(
        "bars.*.hide_empty_workspaces",
        "Hide empty",
        "Hide workspaces with no windows, except the active one."
    ),
    e!(
        "bars.*.show_capsule",
        "Capsule",
        "Draw a capsule (pill) background behind the active workspace indicator."
    ),
    e!(
        "bars.*.show_active_indicator",
        "Active indicator",
        "Mark the active window with an indicator dot."
    ),
    e!(
        "bars.*.scroll_to_switch",
        "Wheel switches",
        "Mouse wheel over the bar switches workspaces."
    ),
    e!(
        "bars.*.scroll_wrap",
        "Wheel wraps",
        "Wheel switching wraps from the last workspace back to the first, and vice versa."
    ),
    e!(
        "bars.*.scroll_reverse",
        "Reverse wheel",
        "Reverse the wheel direction for workspace switching."
    ),
    e!(
        "bars.*.scroll_repeat_ms",
        "Wheel repeat ms",
        "Minimum milliseconds between wheel-triggered workspace switches. Higher stops one scroll flick skipping several workspaces."
    ),
    // -- Layers -----------------------------------------------------------------------
    e!(
        "gamepad.navi.layers.*.hold",
        "Hold buttons",
        "Buttons that must all be held to activate this layer. While held, the layer's bindings replace the base navi bindings."
    ),
    e!(
        "ui.layer.hold_axes",
        "Hold axis",
        "Analog axis and pull threshold (0–1) that must also be held to activate this layer, e.g. ABS_Z at 0.35 for a half-pulled trigger."
    ),
    // -- Style colours ------------------------------------------------------------------
    e!(
        "style.background",
        "Background",
        "Popup background colour (hex, e.g. #101216)."
    ),
    e!(
        "style.background_end",
        "Background end",
        "Second colour of the popup background gradient."
    ),
    e!(
        "style.surface",
        "Surface",
        "Workspace and window tile background colour."
    ),
    e!(
        "style.surface_hover",
        "Surface hover",
        "Tile background colour under the pointer."
    ),
    e!("style.border", "Border", "Panel and tile border colour."),
    e!(
        "style.border_soft",
        "Soft border",
        "Subtler border colour used inside tiles."
    ),
    e!(
        "style.accent",
        "Accent",
        "Accent colour for selection and active indicators."
    ),
    e!(
        "style.selection_border",
        "Selection",
        "Border colour of the selected tile."
    ),
    e!("style.text", "Text", "Primary label text colour."),
    e!(
        "style.muted",
        "Muted",
        "Secondary, de-emphasised text colour."
    ),
    e!(
        "style.urgent",
        "Urgent",
        "Colour used for urgent (attention-demanding) workspaces."
    ),
    e!(
        "style.shadow",
        "Shadow",
        "Popup drop-shadow colour, usually with alpha."
    ),
    e!(
        "style.floating_backing",
        "Floating backing",
        "Backing colour behind floating windows in the popup."
    ),
    // -- Style geometry -------------------------------------------------------------------
    e!(
        "style.panel_radius",
        "Panel radius",
        "Corner radius of the popup panel in logical pixels."
    ),
    e!(
        "style.radius",
        "Small radius",
        "Corner radius of tiles and small elements in logical pixels."
    ),
    e!(
        "style.border_width",
        "Border width",
        "Border thickness in logical pixels."
    ),
    e!(
        "style.dim_alpha",
        "Dim alpha",
        "Opacity (0–1) of the dimmed backdrop behind the popup. 0 is transparent, 1 is solid."
    ),
    e!(
        "style.panel_padding",
        "Panel padding",
        "Inner padding of the popup panel in logical pixels."
    ),
    e!(
        "style.gap",
        "Gap",
        "General gap between popup elements in logical pixels."
    ),
    e!(
        "style.canvas_gap",
        "Canvas gap",
        "Gap between output canvases in logical pixels when Outputs is combined."
    ),
    e!(
        "style.column_gap",
        "Column gap",
        "Gap between window columns in logical pixels."
    ),
    e!(
        "style.stack_gap",
        "Stack gap",
        "Gap between stacked windows in a column, in logical pixels."
    ),
    e!(
        "style.row_gap",
        "Row gap",
        "Gap between workspace rows in logical pixels."
    ),
    e!(
        "style.shadow_offset",
        "Shadow offset",
        "Vertical drop-shadow offset in logical pixels. Negative moves the shadow up."
    ),
    e!(
        "style.shadow_spread",
        "Shadow spread",
        "Drop-shadow blur/spread radius in logical pixels."
    ),
    e!(
        "style.empty_dot_radius",
        "Empty dot",
        "Radius of the dot marking an empty workspace, in logical pixels."
    ),
    e!(
        "style.label_font_size",
        "Label size",
        "Workspace label font size in logical pixels."
    ),
    e!(
        "style.title_font_size",
        "Title size",
        "Window title font size in logical pixels."
    ),
    e!(
        "style.urgent_ring_width",
        "Urgent ring",
        "Urgent ring thickness in logical pixels (Urgent indicator: ring)."
    ),
    e!(
        "style.urgent_ring_padding",
        "Urgent padding",
        "Gap between a tile and its urgent ring, in logical pixels."
    ),
    e!(
        "style.urgent_glow_spread",
        "Urgent spread",
        "Urgent glow spread radius in logical pixels (Urgent indicator: glow)."
    ),
    e!(
        "style.urgent_glow_alpha",
        "Urgent alpha",
        "Urgent glow opacity (0–1)."
    ),
    e!(
        "style.urgent_badge_radius",
        "Urgent badge",
        "Urgent badge radius in logical pixels (Urgent indicator: badge)."
    ),
    e!(
        "ui.bindings.xmb",
        "XMB buttons",
        "Bindings used while the XMB popup is open: up/down/left/right/tab/back/confirm/close/open."
    ),
    e!(
        "ui.bindings.navi_base",
        "Navi base",
        "Bindings used on the desktop in navi mode when no hold layer is active: pointer clicks, scroll, xmb.open, and niri.* actions."
    ),
    e!(
        "ui.bindings.layer",
        "Bindings",
        "Bindings in this layer, used instead of Navi base while the layer is held."
    ),
    e!(
        "ui.card.detected_controller",
        "Detected controller",
        "Controllers the running daemon currently sees. This is live status, not config."
    ),
    e!(
        "ui.card.controller_map",
        "Controller map",
        "Click a control on the controller picture to edit its binding in the inspector."
    ),
    e!(
        "ui.card.xmb_panel",
        "XMB panel",
        "Live popup render using the edited config. Hover previews emphasis; clicking a window or workspace focuses it."
    ),
    e!(
        "ui.card.model",
        "Model",
        "Counts from the edited config model."
    ),
    // -- Sections, cards, headings ---------------------------------------------------------
    e!(
        "ui.section.controller",
        "Controller",
        "Gamepad device, pointer, bindings, and layers."
    ),
    e!(
        "ui.section.trigger",
        "Trigger Lab",
        "Analog-trigger response curve and LT/RT speed tuning."
    ),
    e!(
        "ui.section.popup",
        "Popup",
        "XMB popup panel, behaviour, peek, animation, theme, colours, and bars."
    ),
    e!(
        "ui.section.preview",
        "Preview",
        "Live daemon snapshot rendered with the edited config."
    ),
    e!(
        "ui.card.device",
        "Device",
        "Which controllers are read and how game/navi mode works."
    ),
    e!(
        "ui.card.pointer",
        "Pointer",
        "Stick-driven pointer motion and scrolling in navi mode."
    ),
    e!(
        "ui.card.bindings",
        "Bindings",
        "Button-to-action maps. XMB buttons apply while the popup is open; Navi base applies on the desktop."
    ),
    e!(
        "ui.card.layers",
        "Layers",
        "Extra binding maps active only while their hold buttons or axes are held, like a modifier key."
    ),
    e!(
        "ui.card.response",
        "Response",
        "Trigger response graph: pull on the horizontal axis, shaped output on the vertical. Double-click to add a point, drag to move, right-click to delete."
    ),
    e!(
        "ui.card.tune",
        "Tune",
        "Trigger pedals simulation and numeric speed-accel settings."
    ),
    e!(
        "ui.card.panel",
        "Panel",
        "Popup size, position, and which outputs it covers."
    ),
    e!(
        "ui.card.placements",
        "Placements",
        "Per-output anchored positions, overriding Position for listed outputs."
    ),
    e!(
        "ui.card.behavior",
        "Behavior",
        "Popup input, closing, and drag-and-drop behaviour."
    ),
    e!(
        "ui.card.peek",
        "Peek",
        "Passive overlay shown during Niri keybind navigation."
    ),
    e!(
        "ui.card.animation",
        "Animation",
        "Popup animation durations and opening scale."
    ),
    e!(
        "ui.card.theme",
        "Theme",
        "External theme file merged over the built-in style."
    ),
    e!(
        "ui.card.colors",
        "Colors",
        "Style colours as hex values. The square shows the parsed colour."
    ),
    e!(
        "ui.card.geometry",
        "Geometry",
        "Style sizes, gaps, and radii in logical pixels unless an entry says otherwise."
    ),
    e!(
        "ui.card.bars",
        "Bars",
        "Built-in per-output workspace bars. Disabled in many setups, where the Noctalia dock plugin draws the strip instead."
    ),
    e!(
        "ui.heading.motion",
        "Motion",
        "Settings for moving the pointer with an analog stick."
    ),
    e!(
        "ui.heading.scroll",
        "Scroll",
        "Settings for scrolling with an analog stick."
    ),
    e!(
        "ui.heading.trigger",
        "Trigger",
        "Shared LT/RT trigger shaping used by both pointer and scroll speed modulation."
    ),
    e!(
        "ui.heading.pointer_accel",
        "Pointer accel",
        "Trigger speed modulation for pointer motion."
    ),
    e!(
        "ui.heading.scroll_accel",
        "Scroll accel",
        "Trigger speed modulation for scrolling."
    ),
    e!(
        "ui.heading.input",
        "Input",
        "How popup navigation and dragging behave."
    ),
    e!(
        "ui.heading.close_when",
        "Close when",
        "Events that close the manual popup."
    ),
    e!(
        "ui.heading.drop_behavior",
        "Drop behavior",
        "What happens when a window is dropped in the popup."
    ),
    // -- Actions, statuses, misc UI ------------------------------------------------------------
    e!(
        "ui.action.load",
        "Load",
        "Load the TOML file at the path above, discarding the in-memory model."
    ),
    e!(
        "ui.action.changes",
        "Changes",
        "Show or hide the list of pending config paths and the validated TOML."
    ),
    e!(
        "ui.action.preview",
        "Preview",
        "Go to the Preview section with the live popup render."
    ),
    e!(
        "ui.action.validate",
        "Validate",
        "Check the edited config with the daemon's config checker without saving."
    ),
    e!(
        "ui.action.save",
        "Save",
        "Validate and write the config file. Does not restart the daemon."
    ),
    e!(
        "ui.action.save_restart",
        "Save & Restart",
        "Save, then restart the daemon so the new config takes effect."
    ),
    e!(
        "ui.action.revert",
        "Revert",
        "Discard all edits and restore the last loaded or saved config."
    ),
    e!(
        "ui.action.dismiss",
        "Dismiss",
        "Hide this error message. The error itself is not fixed by dismissing it."
    ),
    e!(
        "ui.action.view_toml",
        "View TOML",
        "Show the validated TOML that Save would write."
    ),
    e!(
        "ui.action.copy_toml",
        "Copy TOML",
        "Copy the validated TOML to the clipboard."
    ),
    e!(
        "ui.action.add_binding",
        "Add binding",
        "Bind another standard button in this map. Only unbound buttons are offered."
    ),
    e!(
        "ui.action.add_layer",
        "Add layer",
        "Add a hold-to-activate binding layer."
    ),
    e!(
        "ui.action.add_hold_axis",
        "Add hold axis",
        "Add an analog axis (e.g. a trigger) that must be pulled past its threshold to hold this layer."
    ),
    e!(
        "ui.action.add_placement",
        "Add placement",
        "Add a per-output anchored popup position."
    ),
    e!(
        "ui.action.add_bar",
        "Add bar",
        "Add a built-in workspace bar."
    ),
    e!(
        "ui.action.pin_preview",
        "Pin",
        "Pin the live preview above the editor so it stays visible while scrolling. Unpin returns it to the page."
    ),
    e!(
        "ui.action.new_theme",
        "+ New theme",
        "Create a new valid theme file beside the config with seeded defaults and select it."
    ),
    e!(
        "ui.action.delete",
        "Delete",
        "Delete this entry. The change is only written on Save."
    ),
    e!(
        "ui.action.remove",
        "Remove",
        "Remove this button from the list."
    ),
    e!("ui.action.cancel", "Cancel", "Close without saving."),
    e!(
        "ui.action.add_point",
        "+ Point",
        "Add a response point to the trigger graph (at most five)."
    ),
    e!(
        "ui.action.linear",
        "Linear",
        "Reset the trigger response to a straight line (curve 1.0, no custom points)."
    ),
    e!(
        "ui.action.convert_curve",
        "Convert curve",
        "Replace the curve exponent with editable points sampled from it."
    ),
    e!(
        "ui.action.clear_points",
        "Clear",
        "Remove all custom response points and use the curve exponent again."
    ),
    e!(
        "ui.action.delete_point",
        "Delete point",
        "Remove this response point from the graph."
    ),
    e!(
        "ui.action.reset_sim",
        "Reset",
        "Reset the simulated trigger pedals to rest."
    ),
    e!(
        "ui.inspector.context",
        "Context",
        "Which binding map the Action below edits: base (desktop navi), xmb (popup open), or a named hold layer."
    ),
    e!(
        "ui.inspector.action",
        "Action",
        "What the selected button does in this context. unbound removes the binding."
    ),
    e!(
        "ui.inspector.selected",
        "Selected",
        "The controller control selected on the map. Click another control to edit it."
    ),
    e!(
        "ui.inspector.bindings_in_context",
        "Bindings in context",
        "All bindings in the selected context, button → action."
    ),
    e!(
        "ui.status.no_changes",
        "No changes",
        "The edited model matches the loaded file."
    ),
    e!(
        "ui.status.no_bindings",
        "No bindings",
        "This map is empty. Add a binding below."
    ),
    e!(
        "ui.status.no_layers",
        "No layers",
        "No hold layers defined. Add one below."
    ),
    e!("ui.status.no_bars", "No bars", "No built-in bars defined."),
    e!(
        "ui.status.no_placements",
        "No placements",
        "No per-output placements. The popup uses Position for every output."
    ),
    e!(
        "ui.status.daemon_offline",
        "daemon offline",
        "The daemon is not answering status requests."
    ),
    e!(
        "ui.status.no_controller",
        "no controller",
        "The daemon is running but no controller is connected."
    ),
    e!(
        "ui.status.not_detected",
        "not detected",
        "No controller was found by the daemon."
    ),
    e!(
        "ui.status.connected",
        "connected",
        "A controller is connected and visible to the daemon."
    ),
    e!(
        "ui.status.no_controller_connected",
        "No controller connected",
        "Connect a controller to test navi bindings and pointer settings live."
    ),
    e!(
        "ui.status.waiting_snapshot",
        "Waiting for niri-xmb live snapshot",
        "The preview polls the daemon about once a second."
    ),
    e!(
        "ui.status.confirm_restart",
        "Save and restart niri-xmb?",
        "Saving writes the file; restarting replaces the running daemon with the new config."
    ),
    e!(
        "ui.status.confirm_save",
        "Save this TOML?",
        "The file is validated again and replaced atomically."
    ),
    e!(
        "ui.graph.hint",
        "Double-click the graph to add a point; drag points; right-click to delete.",
        "Response graph editing. Points must stay at least 10% of pull apart and outputs must not decrease."
    ),
    e!(
        "ui.graph.lt_brake_rt_gas",
        "LT brake · RT gas · 1.0× baseline",
        "Left trigger slows (brake), right trigger speeds up (gas); 1.0× is the unmodulated baseline speed."
    ),
    e!(
        "ui.sim.offline",
        "Simulated pull · offline",
        "These pedals simulate trigger pull locally. They are not live controller input."
    ),
    e!(
        "ui.app.name",
        "niri-xmb",
        "Application name. Do not translate."
    ),
    e!(
        "ui.app.config",
        "Config",
        "Short subtitle of the config studio window."
    ),
    e!(
        "ui.app.config_studio",
        "Config studio",
        "Name of this editor, shown in the navigation rail."
    ),
    e!(
        "ui.status.error",
        "Error",
        "Heading of the error banner. The message below it is a daemon or validation error, shown untranslated."
    ),
    e!(
        "ui.status.pending_changes",
        "Pending changes",
        "Config paths changed since load. set means written on Save; remove means deleted from the file."
    ),
    e!(
        "ui.status.all_buttons_bound",
        "All standard buttons are bound",
        "Every standard button already has a binding in this map."
    ),
    e!(
        "ui.status.bars_disabled",
        "Bars disabled",
        "The built-in bars are off (Popup → Bars → Enabled). This preview section follows that setting."
    ),
    e!(
        "ui.status.no_outputs",
        "No outputs in live snapshot",
        "The daemon snapshot lists no monitors, so there is nothing to render."
    ),
    e!(
        "ui.status.popup_render_failed",
        "Popup could not be rendered",
        "The offline renderer returned no frame for the current config and snapshot."
    ),
    e!(
        "ui.status.preview_config_error",
        "Preview cannot read the current config",
        "The edited model does not parse as a valid config, so the preview cannot render. Validate shows the error."
    ),
    e!(
        "ui.trigger.shape",
        "Shape",
        "Trigger response shaping presets. + Point edits the graph freely; the named presets set ready-made point sets."
    ),
    e!(
        "ui.status.off",
        "off",
        "Shown next to a disabled trigger-accel group: its multipliers are not applied."
    ),
    e!(
        "ui.dialog.confirm",
        "Confirm",
        "Title of the save confirmation dialog."
    ),
    e!(
        "ui.preset.soft_start",
        "Soft start",
        "Trigger preset: gentle response early, catching up later. Two points at 35%→10% and 65%→40%."
    ),
    e!(
        "ui.preset.precise",
        "Precise",
        "Trigger preset: very gentle early response for fine speed control, still reaching full pull."
    ),
    e!(
        "ui.preset.snappy",
        "Snappy",
        "Trigger preset: stays gentle briefly, then rises fast to near-full output."
    ),
];

/// Replace dynamic path segments with `*`: numeric indices and layer names.
pub fn normalize_path(path: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in path.split('.') {
        let dynamic =
            part.parse::<usize>().is_ok() || out.last().map(String::as_str) == Some("layers");
        out.push(if dynamic {
            "*".into()
        } else {
            part.to_string()
        });
    }
    out.join(".")
}

/// Catalog entry for a config path, dynamic segments allowed.
pub fn entry_for_path(path: &str) -> Option<&'static Entry> {
    let normalized = normalize_path(path);
    CATALOG.iter().find(|entry| entry.key == normalized)
}

/// First catalog entry with this exact English label.
pub fn entry_for_label(label: &str) -> Option<&'static Entry> {
    CATALOG.iter().find(|entry| entry.label == label)
}

/// Displayed label for a field: catalog by path, else the fallback literal.
pub fn label_for_path<'a>(path: &str, fallback: &'a str) -> &'a str {
    entry_for_path(path).map_or(fallback, |entry| entry.label)
}

/// Hover text for a widget: catalog by config path first, then by label.
pub fn hover_for(path: Option<&str>, label: &str) -> Option<&'static str> {
    path.and_then(entry_for_path)
        .or_else(|| entry_for_label(label))
        .map(|entry| entry.description)
}

/// Hover text by stable `ui.*` key, for non-field widgets.
pub fn hover_for_key(key: &str) -> Option<&'static str> {
    CATALOG
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.description)
}

/// Render the English translation template (`locales/en.json`).
pub fn en_template_json() -> String {
    let messages: Map<String, Value> = CATALOG
        .iter()
        .map(|entry| {
            (
                entry.key.to_string(),
                json!({"label": entry.label, "description": entry.description}),
            )
        })
        .collect();
    let doc = json!({
        "locale": "en",
        "fallback": "en",
        "messages": Value::Object(messages),
    });
    serde_json::to_string_pretty(&doc).expect("template serializes") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalog_keys_are_unique_and_complete() {
        let mut seen = BTreeSet::new();
        for entry in CATALOG {
            assert!(seen.insert(entry.key), "duplicate key {}", entry.key);
            assert!(!entry.label.is_empty(), "empty label for {}", entry.key);
            assert!(
                entry.description.len() > 20 && entry.description.ends_with('.'),
                "description for {} must be a full sentence: {:?}",
                entry.key,
                entry.description
            );
        }
    }

    #[test]
    fn screenshot_pointer_terms_have_hover_text() {
        // The terms called out as not immediately understandable.
        for (path, label) in [
            ("gamepad.navi.pointer.stick", "Stick"),
            ("gamepad.navi.pointer.deadzone", "Deadzone"),
            ("gamepad.navi.pointer.curve", "Curve"),
            ("gamepad.navi.pointer.speed", "Speed"),
            ("gamepad.navi.pointer.max_step", "Max step"),
            ("gamepad.navi.pointer.invert_y", "Invert Y"),
        ] {
            let entry = entry_for_path(path).expect(path);
            assert_eq!(entry.label, label, "{path}");
            assert_eq!(hover_for(Some(path), label), Some(entry.description));
        }
    }

    #[test]
    fn dynamic_paths_normalize_to_catalog_keys() {
        assert_eq!(normalize_path("bars.2.output"), "bars.*.output");
        assert_eq!(
            normalize_path("gamepad.navi.layers.monitor.hold"),
            "gamepad.navi.layers.*.hold"
        );
        assert!(entry_for_path("bars.0.scroll_repeat_ms").is_some());
        assert!(entry_for_path("popup.placements.1.margin").is_some());
    }

    #[test]
    fn catalog_covers_field_paths_used_in_main_rs() {
        let src = include_str!("main.rs");
        // Non-field paths that share a config prefix (maps, model indexing).
        const NON_FIELD: &[&str] = &[
            "gamepad.buttons",
            "gamepad.navi.bindings",
            "gamepad.navi.layers",
            "gamepad.navi.pointer",
            "gamepad.navi.pointer.trigger_points",
            "gamepad.navi.pointer.trigger_deadzone",
            "gamepad.navi.pointer.accel",
            "gamepad.navi.pointer.scroll_accel",
            "popup.placements",
            "gamepad.mode",
            // A layer name from a patch test fixture, not a field.
            "gamepad.navi.layers.workspace",
        ];
        let mut missing = Vec::new();
        let mut i = 0;
        let bytes = src.as_bytes();
        while i < bytes.len() {
            if bytes[i] == b'"' {
                let start = i + 1;
                let mut j = start;
                while j < bytes.len() && bytes[j] != b'"' {
                    j += 1;
                }
                let literal = &src[start..j];
                let is_path = literal.contains('.')
                    && !literal.contains(['{', ' ', '/'])
                    && literal.split('.').all(|p| {
                        p.chars()
                            .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit())
                    });
                let fieldish = [
                    "gamepad.",
                    "behavior.",
                    "popup.",
                    "peek.",
                    "animation.",
                    "style.",
                ]
                .iter()
                .any(|prefix| literal.starts_with(prefix))
                    || literal == "theme"
                    || literal == "bars_enabled";
                if is_path && fieldish && !NON_FIELD.contains(&literal) {
                    // Trigger paths are covered concretely, not by format base.
                    if entry_for_path(literal).is_none() {
                        missing.push(literal.to_string());
                    }
                }
                i = j;
            }
            i += 1;
        }
        missing.sort();
        missing.dedup();
        assert!(
            missing.is_empty(),
            "field paths missing from i18n catalog: {missing:?}"
        );
    }

    #[test]
    fn locales_en_json_matches_catalog() {
        let path = format!("{}/locales/en.json", env!("CARGO_MANIFEST_DIR"));
        let expected = en_template_json();
        if std::env::var_os("UPDATE_LOCALES").is_some() {
            std::fs::create_dir_all(format!("{}/locales", env!("CARGO_MANIFEST_DIR")))
                .expect("create locales dir");
            std::fs::write(&path, &expected).expect("write locales/en.json");
            return;
        }
        let actual = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!("read {path}: {error}; regenerate with UPDATE_LOCALES=1 cargo test -p niri-xmb-config-studio i18n")
        });
        assert_eq!(
            actual, expected,
            "locales/en.json is stale; regenerate with UPDATE_LOCALES=1 cargo test -p niri-xmb-config-studio i18n"
        );
    }
}
