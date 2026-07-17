# niri-xmb

`niri-xmb` is a Niri-native workspace bar and icon-only workspace navigator. It
runs independently of Noctalia, so Noctalia upgrades do not need to carry a
custom widget patch.

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
- `popup.position`: `"cursor"`, `"center"`, or `"fixed"`.

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
niri-xmb quit
```

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
