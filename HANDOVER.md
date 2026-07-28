# niri-xmb XMB popup handover

## Objective

Turn the Mod+Z popup into a compact XMB-style navigator: one small canvas per
monitor, fixed focal point, icon-only presentation, smooth staged or live
navigation, and a passive workspace-change peek overlay.

## Current work queue

- [x] Restore the custom Niri smart workspace move on `Mod+Shift+Up` and
  `Mod+Shift+Down`. The regression was in the active bind file. Both Niri
  branches still contain commit `62cfdc3f`. Keep this commit during upstream
  merges. The action moves inside a column first. At a column edge, the action
  inserts a new workspace above or below and moves the window to it.
- [x] Make the shell `niri` command use the deployed custom binary. The
  `~/.local/bin/niri` link now targets the active custom prefix.
- [x] Consolidate Niri into the clean canonical checkout at
  `/home/ez/Projects/niri`. The old clean build tree and damaged worktree are
  preserved under `~/.local/state/dotfiles/retired-20260729/sources/`.
- [x] Add dynamic popup surfaces for outputs connected after daemon startup.
  This lets Sunshine's `Virtual-1` output join the XMB without a daemon restart.
- [x] Finish the hidden popup surface lifecycle. Hidden popups now destroy the
  layer role and `wl_surface`. The daemon keeps SHM buffers for fast remapping.
- [x] Commit and push the pending niri-xmb popup input and hidden-surface fixes.
- [x] Merge current Noctalia upstream `main` into the Noctalia fork. Preserve
  the custom taskbar output target, icon, drag source, and drop routing changes.
- [x] Run the Noctalia v5 test suite and build the desktop package. All 62 tests
  pass. The build ran only on the desktop.
- [x] Build a portable Noctalia v5 package. The binary uses
  `$ORIGIN/../lib`, and the private libraries are bundled in the prefix.
- [x] Back up the desktop Noctalia v4 configuration. The frozen backup is
  `/home/ez/.local/share/noctalia-rollback/v4-desktop-20260728`.
- [x] Create the desktop Noctalia v5 configuration. One top bar on `HDMI-A-2`
  contains two taskbar widgets. Each widget has an explicit output target and
  `niri-xmb` drag routing.
- [x] Remove v4-only plugin entries from active v5 configuration. Disable the
  old recorder key bindings until native replacements are selected.
- [x] Disable Neowall autostart. Use the native Noctalia v5 wallpaper function.
- [x] Replace the desktop v4 startup command with the native Noctalia v5
  startup command.
- [ ] Validate both bars, dock input, taskbar input, XMB drag and reorder,
  `Mod+Z`, tray, audio, network, notifications, lock, and wallpaper.
- [ ] Push a Noctalia fork checkpoint after the remaining manual checks pass.

## Current product decisions

- The popup is compact, not fullscreen; Niri's overview remains the fullscreen
  overview.
- Each output owns a canvas. With combined output mode the canvases are placed
  side by side inside one panel.
- Workspaces scroll vertically and windows scroll horizontally beneath a fixed
  centre point.
- Selection is indicated only by centre position, larger icon size, brightness,
  and motion. There is no selection or focal glow.
- The deployed config uses one combined panel with all output maps. The panel
  follows the active output and stays at the top centre of that output.
- The same top-centre placement applies to Sunshine's `Virtual-1` output in
  the Moonlight view.
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
- Kept the daemon and its per-output surfaces resident; `show`/`toggle` are
  control-socket requests and no longer wait for a compositor roundtrip.
- Added compositor frame-callback backpressure, reusable preallocated SHM
  buffers, renderer-provided regional damage, and region-only RGBA conversion.
- Cropped the opening animation layer to the panel/shadow bounds instead of
  scaling a full-monitor transparent pixmap.
- Replaced the fixed polling loop with Wayland-or-producer wakeups and
  deadline-based waits; settled visible and hidden CPU measured 0.00%.
- Deduplicated metadata-only window updates so title spinners do not redraw.
- Removed all wave configuration/rendering and all focal/selection glow code.
- Added backward-compatible percentage strings for popup caps, canvas sizes,
  fixed coordinates, and per-output margins. Numeric values remain logical
  pixels and percentages resolve against each host output.
- `print-default-config` now emits the fully commented maintained template.
- Added `niri-xmb restart`: validate config/theme first, leave a running daemon
  untouched on validation errors, then gracefully stop, relaunch through Niri,
  and verify the replacement control socket.

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
- `cargo test --workspace` — 48 passed
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo build --release -p niri-xmb`
- `git diff --check`
- Search confirmed no compiled wave or glow configuration remains.
- Local desktop deployment: restart 0.08 s; `status` and `toggle` below the
  timer's 0.01 s resolution; warm opening peaked near 29% of one core.
- Six-second settled samples measured 0.00% CPU both visible and hidden, with
  approximately 43 MB RSS after the popup buffers were exercised.
- Noctalia v4 was restarted cleanly and loaded `niri-xmb-bar` without QML
  errors. Pointer interaction still needs one manual widget/dock confirmation.
- Hidden lifecycle verification on 2026-07-28 showed zero `niri-xmb-popup`
  entries in `niri msg layers`. Showing created one entry per output. Hiding
  removed both entries again.
- Ten show/hide cycles completed successfully. The 20 local control calls took
  0.04 seconds. The daemon returned to hidden mode with approximately 43 MB RSS.


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
