# niri-xmb XMB popup handover

## Objective

Turn the Mod+Z popup into a compact XMB-style navigator: one small canvas per
monitor, fixed focal point, icon-only presentation, smooth staged or live
navigation, and a passive workspace-change peek overlay.

## Current product decisions

- The popup is compact, not fullscreen; Niri's overview remains the fullscreen
  overview.
- Each output owns a canvas. With combined output mode the canvases are placed
  side by side inside one panel.
- Workspaces scroll vertically and windows scroll horizontally beneath a fixed
  centre point.
- Selection is indicated only by centre position, larger icon size, brightness,
  and motion. There is no selection or focal glow.
- Popup placement is anchored per output. The deployed config places
  `HDMI-A-1` on the right and `HDMI-A-2` on the left, both vertically centred,
  so the two panels face inward.
- The animated wave background was rejected for this panel and is not compiled.
  Its CPU prototype is retained at `reference/xmb_wave.rs` for possible reuse in
  a wallpaper/background engine.

## Completed implementation

- Reworked popup rendering into the XMB cross layout with edge fading,
  focused-icon scaling, dimmed inactive rows, content-sized panel chrome, and
  optional labels/titles.
- Preserved Niri's column topology: columns run horizontally and windows joined
  into the same column stack vertically, with two-axis focal-point scrolling.
- Added animation state in `crates/niri-xmb/src/anim.rs` for panel presence,
  per-output vertical scroll, per-workspace horizontal scroll, and emphasis.
  Per-workspace scroll tracks prevent a correctly positioned row from sliding
  in again when it becomes selected.
- Added session-local selection memory. Deferred navigation remembers the
  staged window in every visited workspace until confirm or close.
- Added `behavior.navigation = "deferred" | "live"`.
- Added passive peek mode with configurable linger duration. Peek is
  non-interactive, workspace-only by default, and is suppressed for Niri's
  overview, cross-monitor pointer focus, newly opened/closed windows, and focus
  changes caused by niri-xmb itself.
- Unified manual and peek state: external workspace hotkeys re-sync an open
  manual panel as well as the passive overlay. Manual mode always observes
  window focus, even when peek remains configured for workspace changes only.
- Added per-output anchored placement via `[[popup.placements]]`.
- Added config-driven colours, dimensions, spacing, radii, and animation
  durations. Theme files are merged over `[style]` key by key.
- Added Alacritty-derived themes in `themes/dank.toml` and
  `themes/noctalia.toml`; the deployed config uses the dank theme.
- Reused rendering pixmaps and skipped already-blank popup surfaces to reduce
  allocation/upload stalls.
- Removed all wave configuration/rendering and all focal/selection glow code.

## Relevant files

- `crates/niri-xmb/src/main.rs`: popup/peek state machine, focus classification,
  selection memory, navigation modes, and redraw loop.
- `crates/niri-xmb/src/render.rs`: XMB canvas layout, anchored placement, icon
  rendering, hit zones, and reusable buffers.
- `crates/niri-xmb/src/anim.rs`: animation tracks and timing.
- `crates/niri-xmb/src/config.rs`: popup placement, peek, navigation, animation,
  style, and theme loading.
- `crates/niri-xmb/src/model.rs`, `niri.rs`: active-window and overview state.
- `crates/niri-xmb/src/wayland.rs`: passive versus interactive popup input.
- `crates/niri-xmb/src/draw.rs`, `text.rs`, `icons.rs`: standalone rendering,
  font, system icon-theme, and desktop-entry resolution.
- `config/niri-xmb.toml.example`, `README.md`: public configuration/docs.
- `themes/`: shipped theme examples.
- `reference/xmb_wave.rs`: archived, non-compiled wave prototype.

## Verification

Verified on 2026-07-28:

- `cargo fmt --all`
- `cargo test --workspace` — 39 passed
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --release -p niri-xmb`
- `git diff --check`
- Search confirmed no compiled wave or glow configuration remains.

## Deployment state

- Installed binary: `/home/ez/.local/bin/niri-xmb`
- Active config: `/home/ez/.config/niri-xmb/config.toml`
- Theme: `~/.config/niri-xmb/themes/dank.toml`
- Daemon is launched through Niri so it persists with the desktop session.
- `niri-xmb status` reported mode `hidden`, generation `1507`, and a healthy
  control socket.
- Peek is enabled for 1500 ms and `on_window_focus = false`.

## Constraints and workspace notes

- `target-overview-fix/` is an untracked generated build directory (~854 MiB).
  It is excluded by `.gitignore` and is not part of the checkpoint.
- `reference/` and `themes/` are intentional checkpoint source directories.

## Remaining tasks

Live compositor checks completed:

- External `focus-column-left` moved the open manual panel from window 27 to
  window 5 and translated the row under the fixed focal point.
- Windows 5 and 27 were temporarily joined into one column. They rendered as a
  vertical stack; `focus-window-down` centred window 27 vertically. The window
  was then expelled and the original three-column layout and window 27 focus
  were restored.
- Inward anchors are confirmed: right on `HDMI-A-1`, left on `HDMI-A-2`.
- The popup was left hidden.

Remaining UX checks:

1. Confirm the centred, enlarged icon is sufficient without any glow.
2. Decide whether Mod+Z left/right traversal through a vertical stack feels
   natural. It currently walks the flattened window order, so moving between
   members of one column scrolls vertically while Up/Down remain workspace
   navigation.
3. Watch for horizontal row slide when changing workspaces.
4. Confirm peek appears only for workspace navigation, not pointer crossings,
   dialogs, or Niri's overview.
5. After desktop UX settles, decide whether to deploy this popup redesign to
   the laptop or keep the laptop on the smaller v5-focused checkpoint.
