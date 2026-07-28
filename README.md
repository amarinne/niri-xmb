# niri-xmb

`niri-xmb` is a Niri-native icon-only workspace navigator with optional bar
frontends. Its popup and Niri rearrangement backend run independently of
Noctalia. The repository also ships an actual Quickshell/Noctalia v4 bar plugin
and a native Noctalia v5 taskbar configuration.

The project is adapted from Woven's native Smithay Client Toolkit/tiny-skia
layer-shell implementation. It consumes Niri's event stream directly and uses
Niri IPC actions for focusing and moving windows.

## Current features

- One explicitly configured bar surface per output.
- Dynamic workspace and window updates from Niri's event stream.
- Shortcut-controlled popup for one output, separate output panels, or a combined multi-output map.
- Centered, fixed, or cursor-relative popup positioning.
- Icon-only tiling layout map with normalized floating-window positions.
- Drag windows between workspaces, outputs, and left/right column positions.
- Optional center drop zone to join a column, configured per output.
- Drop on empty map space to make/reposition a floating window.
- Keyboard navigation, focus restoration after drag, urgency, and app icons.
- PNG icons natively, with optional SVG rendering through ImageMagick.
- A `drop` control command that lets shell widgets reuse the same tiled/floating
  preserving Niri rearrangement logic.
- Noctalia v4 and v5 bar ports that replace the standalone layer-shell bars.

The popup is implemented as independent transparent layer-shell surfaces. This
is deliberate: Noctalia v5's plugin item API does not expose the pointer motion
and release events needed for cross-workspace drag and drop.

## Build

Requirements are a recent Rust toolchain, Niri, and a compositor exposing
`wlr-layer-shell`. ImageMagick's `magick` command is optional but recommended for
SVG-only application icons.

```bash
cd /home/ez/Projects/niri-xmb
cargo build --release -p niri-xmb
install -Dm755 target/release/niri-xmb ~/.local/bin/niri-xmb
```

## Configure

The default configuration path is `~/.config/niri-xmb/config.toml`. Start from
the included example:

```bash
mkdir -p ~/.config/niri-xmb
cp config/niri-xmb.toml.example ~/.config/niri-xmb/config.toml
niri msg --json outputs
```

Set each `output` to the connector name reported by Niri. A bar is never moved
to a different monitor automatically.

Important behavior:

- `bars_enabled = false`: do not create standalone bar surfaces. Use this when
  either Noctalia bar port is installed; the popup daemon remains active.
- `allow_column_join = false`: every icon drop is split into left and right
  halves. The window becomes its own column on the chosen side.
- `allow_column_join = true`: the middle third joins the target column; the
  outer thirds insert to its left or right.
- `show_workspace_labels`: add numeric workspace labels beside the icons. The
  default icon-only style keeps them hidden.
- `hide_empty_workspaces`: omit empty, inactive workspaces from that bar.
- `show_capsule` and `show_active_indicator`: independently control the group
  background and the small focused-workspace dot.
- `scroll_to_switch`, `scroll_wrap`, and `scroll_reverse`: control workspace
  switching with the wheel over each bar.
- `popup.outputs = "focused"`: show one popup on the focused monitor.
- `popup.outputs = "all"`: show a separate local workspace map on every
  monitor at the same time.
- `popup.outputs = "combined"`: show one panel on the focused monitor with all
  outputs side by side. Workspaces are rows from top to bottom, while each
  workspace's Niri columns run left to right. The panel grows to the widest
  strip and largest workspace count, up to the configured maximum size.
- `popup.surface_mode = "fixed"`: keep the popup on the monitor where it was
  opened.
- `popup.surface_mode = "follow_active"`: move the popup after a different
  monitor remains actively focused for `follow_delay_ms`. Active dragging and
  Niri's temporary rearrangement focus are ignored.
- `popup.surface_mode = "mirror"`: show the popup on every monitor. With
  `outputs = "combined"`, every monitor receives the same combined organizer.
- `popup.position`: `"anchored"`, `"cursor"`, `"center"`, or `"fixed"`.
  Anchored placement can choose left/centre/right and top/centre/bottom per
  output with `[[popup.placements]]`.
- `popup.width` and `popup.height` are maximums, not fixed sizes. The panel
  measures its icons and shrinks to fit.
- `behavior.snap_cursor_after_drop = false`: restore the pointer to its release
  position after Niri's temporary focus step used for exact tiled placement.

### Popup appearance

The popup is icon-only by default: Niri's overview already provides the
full-screen, text-heavy view, so this panel stays compact and carries state
through position, scale, and brightness instead of nested boxes and labels.

Each monitor gets its own small canvas and the canvases sit side by side. A
canvas is a cross with a **fixed focal point at its centre**: workspaces run
vertically through it, and the selected workspace's windows run horizontally
through it. Moving the selection scrolls the content under the focal point
rather than moving a cursor around the panel, so the eye never has to search
for the selection. Rows that are not selected park their *active* window on the
focal column, so that column always shows what activating each workspace would
land on. Icons fade out toward the canvas edge instead of being clipped.

- `popup.canvas_width` / `popup.canvas_height`: size of one monitor's canvas.
  `popup.width` and `popup.height` only cap the overall panel.
- `popup.show_labels = true`: add workspace and output names. Off by default.
- `popup.show_titles = true`: add the title of the *selected* window only,
  rather than a caption under every icon.
### Theming

The popup's palette and primary proportions—icon sizes, cross spacing, panel
padding/radius, shadow, and transition timing—come from config. Small internal
safety bounds and text insets remain implementation details. Start from the
current defaults with `niri-xmb print-default-config`, which always reflects the
running build.

Highlights:

- `style.background` and `style.background_end`: top and bottom stops of the
  panel's vertical gradient. Set them equal for a flat panel.
- `style.icon_size` and `style.icon_size_focused`: the selected window is drawn
  larger, which is what marks selection in place of an outline.
- `style.dim_alpha`: opacity of icons outside the selected workspace. Raise it
  toward `1.0` for a flatter look, lower it for more focus separation.
- `style.column_gap` and `style.row_gap`: added to `icon_size_focused` to get
  the spacing of the cross. These set how far each keypress scrolls.
- `style.stack_gap`: spacing between windows joined vertically inside one Niri
  column. Columns remain horizontal while their member windows stack vertically.
- `style.shadow`, `shadow_offset`, `shadow_spread`: the panel's drop shadow,
  drawn as two offset passes. Set `shadow` transparent to remove it.
- `[animation]`: `panel_ms`, `emphasis_ms`, and `scroll_ms` are durations, not
  raw easing constants. `enabled = false` applies every change instantly.

Opening the panel, changing the selection, and closing are all animated.
Animation adds redraws only while something is actually moving; once every
track settles, the daemon falls back to its normal idle behaviour rather than
holding a frame loop open.

#### Importing a theme

Set a top-level `theme` key to the path of another TOML file:

```toml
theme = "~/.config/niri-xmb/themes/dank.toml"
```

A leading `~/` expands to `$HOME`; a relative path is resolved against the
directory containing the config file that references it. The theme file only
needs a `[style]` table, and only the keys it wants to change — it is merged
over the main config's `[style]` table **key by key**, with the theme's
values winning. Any style field the theme doesn't mention (radii, sizes,
gaps, font sizes, and any colour it skips) keeps whatever the rest of the
config, or the built-in defaults, already say. A missing or unparsable theme
file is a hard error rather than a silent fallback.

`themes/dank.toml` and `themes/noctalia.toml` ship as examples, derived from
the matching alacritty colour schemes of the same names — each overrides only
the colour fields, leaving layout and sizing at their defaults.

### Navigation

`behavior.navigation` picks how arrow keys relate to Niri's real focus:

- `"deferred"` (default): arrows move a selection inside the popup and nothing
  happens to Niri until Enter or a click confirms it. The popup is a staging
  area you can look around in before committing.
- `"live"`: every arrow key moves Niri's focus straight away, alt-tab style.

In either mode the popup keeps its **own** cursor position for the duration of
a session, rather than mirroring live state. Navigating away from a workspace
and back returns to where you left off, not to whatever Niri happens to be
focusing. The session ends — and the popup snaps back to live state — when the
popup closes or a move is confirmed.

With `navigation = "live"` and `popup.surface_mode = "follow_active"`, focusing
across monitors will also make the popup migrate after `follow_delay_ms`. Set
`surface_mode = "fixed"` if you would rather it stay put.

### Peek overlay

`peek.enabled = true` adds a third, passive mode. Navigate Niri with its own
keybinds and the panel appears by itself, showing where focus just moved, then
hides again. It is a readout, not a control: the surface takes neither pointer
nor keyboard input, so the keybinds you are pressing keep reaching Niri and
clicks pass through to the windows underneath.

- `peek.duration_ms`: how long it lingers after the last focus change. The
  countdown restarts on every change, so holding down a navigation key keeps
  the overlay up and it disappears shortly after you stop.
- `peek.on_window_focus = false`: only peek on workspace switches, ignoring
  focus moves between windows in the same workspace.

Peek stays out of the way of anything already covering the same ground:

- **Niri's own overview.** Navigating with the overview open raises nothing,
  and an overlay already up is dismissed the moment the overview opens over it.
  A smaller duplicate of the overview is not useful.
- **A session you are driving.** Pressing the popup keybind while a peek
  overlay is up *promotes* it to a normal interactive popup rather than
  dismissing it, and peek never takes over a manual popup.
- **Focus changes niri-xmb caused itself** — confirming a pick, or
  `navigation = "live"` — do not bounce back as a peek for the move just made.

`niri-xmb status` reports `"mode"` as `hidden`, `manual`, or `peek`.

Popup lifetime is configurable under `[behavior]`. The shipped defaults use a
hybrid mode: clicking a window focuses it and closes the popup, while workspace
clicks and drag/drop keep it open for organizing several windows. Set
`close_on_window_click = false` for a persistent organizer, or set
`close_on_workspace_click = true` and `close_on_drop = true` for a fully
transient alt-tab-style popup. Outside click and Escape have independent
switches as well.

Print every default setting with:

```bash
niri-xmb print-default-config
```

## Run and bind in Niri

For a quick foreground test:

```bash
niri-xmb daemon
```

The control commands connect to the running daemon:

```bash
niri-xmb show
niri-xmb hide
niri-xmb toggle
niri-xmb status
niri-xmb drop --source-window-id 10 --target-workspace-id 4 \
  --target-window-id 12 --fraction 0.8
niri-xmb quit
```

## Noctalia bar ports

For Noctalia v4, enable the plugin in
`~/.config/noctalia/plugins/niri-xmb-bar` and place
`"plugin:niri-xmb-bar"` in the desired bar section. Noctalia creates one
instance per configured entry. By default an instance follows its host bar;
set `"targetOutput": "HDMI-A-1"` on an entry to display a different output, or
put two explicitly targeted instances on the same bar. The QML widget supports
app icons, click focus, wheel workspace switching, and left/right drag/drop
through the standalone backend. The maintained source is under
`plugins/noctalia-v4/niri-xmb-bar`.

For Noctalia v5, use the named native taskbar instance in
`ports/noctalia-v5/config.toml.example`. V5 supplies its own per-output grouped
taskbar rendering and sizing. Its native bar currently supports click and wheel
but not Niri rearrangement, so drag/drop remains available in the standalone
popup.

Add the following to the appropriate sections of the Niri config:

```kdl
spawn-at-startup "niri-xmb" "daemon"

binds {
    Mod+F { spawn "niri-xmb" "toggle"; }
}
```

Alternatively, install and enable the user service:

```bash
install -Dm644 niri-xmb.service ~/.config/systemd/user/niri-xmb.service
systemctl --user daemon-reload
systemctl --user enable --now niri-xmb.service
```

Do not use both `spawn-at-startup` and the systemd service.

## Controls

| Input | Action |
| --- | --- |
| Click workspace | Focus workspace |
| Click window | Focus window; popup closes |
| Drag window | Move it to the target workspace/position |
| Up / Down | Select previous/next workspace row |
| Left / Right | Select previous/next window in the row |
| Tab / Shift+Tab | Select the next/previous output in all-output mode |
| Enter | Focus selection; popup closes |
| Escape | Close popup |
| Pointer scroll in popup | Select previous/next workspace |
| Pointer scroll on bar | Focus previous/next workspace, subject to bar settings |

Floating windows use their logical Niri workspace coordinates, normalized into
the popup canvas. Empty-canvas drops preserve the source mode by default: tiled
windows remain tiled and are inserted at the corresponding horizontal position,
while floating windows remain floating and receive new logical coordinates.
Set `empty_drop_floats = true` to opt into converting tiled windows to floating
on empty-canvas drops. Dragging a floating window beside a tiled icon returns it
to tiling.

## Repository layout

Only `crates/woven-common` and `crates/niri-xmb` are members of the active Cargo
workspace. Other retained Woven source files are reference material from the
fork and are not built.

The separately cloned research projects live beside this repository at:

```text
/home/ez/Projects/niri-xmb-references/woven
/home/ez/Projects/niri-xmb-references/niri-taskbar
/home/ez/Projects/niri-xmb-references/niri-minimap
```

## License

GPL-3.0-only, inherited from Woven. See `LICENSE`.
