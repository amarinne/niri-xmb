# Config Studio

Native `egui` editor for the controller and XMB TOML.

## Run

```sh
cargo build -p niri-xmb --bin niri-xmb
cargo build -p niri-xmb-config-studio
cargo run -p niri-xmb -- \
  gui \
  --config ~/.config/niri-xmb/config.toml
```

The `gui` subcommand launches the packaged studio helper and passes it the
selected config and binary paths. Use the installed `niri-xmb gui` command;
the window loads the selected TOML immediately.

Install the helper, icon, and application entry:

```sh
cargo build --release -p niri-xmb-config-studio
scripts/install-config-studio.sh
```

This installs into `~/.local` by default; set `PREFIX=/path` to override. The
entry appears as **niri-xmb Config**.

## Behavior

- **Controller:** slim detected-controller status, the controller map with
  hotspots, the selected-control inspector, and the XMB/Navi binding lists in
  one card, then Device/Pointer and Layers.
- Binding rows pick the button from a dropdown of standard gamepad codes
  (labeled A/Cross, LB/L1, and so on), filtered to buttons not already bound in
  that map. Open combo, mode toggle, and layer hold lists use the same picker,
  so button codes never need typing.
- Selecting a hotspot highlights its row in the matching list (base or XMB,
  following the inspector context) and draws a link from the control to the
  row. A layer context has no link because those bindings live in the Layers
  card.
- **Trigger Lab:** Response (graphs, presets, point inspector) beside Tune
  (pedals, speed multipliers, deadzone/curve/SOCD, pointer/scroll accel).
- **Popup:** panel, placements, behavior grouped into Input / Close when / Drop
  behavior, peek, and animation.
- **Appearance:** theme, colors, geometry, and bar configuration.
- **Live dock:** persistent bottom panel with three tabs. Drag the grip on its
  top edge to resize (the height survives tab switches), double-click the grip
  or press Hide to collapse it to the header. Follow switches tabs with the
  editor section.
  - **Popup:** the daemon's own renderer with the edited config and the live
    status/snapshot, so icons, layout, emphasis, and hit zones match the real
    panel. Hovering highlights the same target; clicking a window or workspace
    sends the existing `focus-window` / `focus-workspace` command.
  - **Controller:** the current template with live pressed-button highlights,
    trigger bars, and each control's Navi/XMB binding plus the Navi layers that
    hold or rebind it. Live state comes from `gamepad.live` in `niri-xmb
    status`.
  - **Keyboard:** the full Niri keybind map read from `~/.config/niri/config.kdl`
    and its includes. The ANSI board keeps normal key proportions (15 key
    widths per row, square caps) and sits beside an aligned Hotkey / Action /
    Source table; it stacks below 880 px. Bound keys are tinted, keys held in
    the studio light up, clicking a key filters the list, and overridden or
    duplicate binds are flagged. The search box filters by hotkey, action, or
    overlay title.
- The top command bar holds the config path, Load, and live daemon status. A
  persistent bottom action bar holds change count/SHA, Changes, Preview,
  Validate, Save, Save & Restart, and Revert. Preview opens the dock's Popup
  tab. Changes opens the patch-path drawer and the validated TOML view.
- Controller hotspots select a control; the inspector edits the active Navi
  base, XMB, or layer binding. Both templates use the same display box.
- Binding rows pick the button from a dropdown of standard gamepad codes
  (labeled A/Cross, LB/L1, and so on), filtered to buttons not already bound in
  that map. Open combo, mode toggle, and layer hold lists use the same picker,
  so button codes never need typing.
- Trigger Lab's compact pedals use the same points and speed-SOCD policy; they
  are simulation, not live controller input. `speed_socd` selects `last`,
  `first`, `cancel`, `multiply`, `prio_decel`, or `prio_accel` for simultaneous
  speed inputs. This does not alter trigger bindings or layer hold_axes.
- Below 700 px the rail becomes a top strip; below 900/1000 px the controller,
  trigger, and appearance grids stack.

## Inline validation

- Every field is validated as it is edited, under the input itself.
  The checks call the daemon's `Config::validate_all`, so messages and
  limits match `config-check` exactly; free-text fields (output names,
  colours, lengths, device paths, axis names, theme file) are checked
  directly so one bad field never hides another.
- Widget ranges match the daemon ranges (bar size depends on edge,
  `scroll_repeat_ms` ≤ 5000, `animation.open_scale` 0.1–2.0, …).
- `Add bar` / `Add placement` seed an unused live output name (falling
  back to `eDP-1`, `HDMI-A-1`, …), `Add layer` seeds a hold axis, and
  `Add hold axis` picks an unused axis, so new items are valid on
  creation instead of failing at save time.
- While any field is invalid, Save and Save & Restart are disabled and
  the action bar counts the invalid fields. Clearing the Theme field
  removes the theme instead of writing an empty path.

## Save

- `Load` reads the TOML and gets the real model from
  `niri-xmb --config PATH config-json`.
- Edits build a patch of changed paths only. Unknown fields and unrelated
  tables remain in the source document.
- `Validate` writes a temporary file beside the config, runs
  `niri-xmb --config TMP config-check`, and shows errors before saving.
- `Save` asks for confirmation, rechecks the loaded SHA-256, validates again,
  writes a same-directory temporary file, preserves the target mode, and
  renames it atomically.
- `Save & Restart` saves first, then invokes the existing
  `niri-xmb --config PATH restart` path. Saving alone does not restart the
  daemon.
- `Revert` restores the last loaded/saved model.

Comments outside edited scalar fields and unknown sections are preserved.
Comments attached to an edited scalar may be normalized by `toml_edit`. The
loaded style includes the theme merge; editing a style field writes the base
`[style]` value while preserving the `theme` path.

`assets/controller-render.png` is the CC0 Xbox Controller.svg render from
Wikimedia Commons.

## Display texts and translations

Every label, heading, button, and status is catalogued in
`crates/niri-xmb-config-studio/src/i18n.rs` with a stable key (config path
for fields, `ui.*` otherwise) and a one-sentence description. That
description is the hover text on the label in the UI, so opaque terms
(Pointer `Stick`/`Deadzone`/`Curve`/`Speed`/`Max step`/`Invert Y`, trigger
`Shape curve`/`Speed SOCD`, peek timings, …) explain themselves on hover.
The generated English template at
`crates/niri-xmb-config-studio/locales/en.json` (same
`{label, description}` shape as the Noctalia plugin translations) is kept
in sync by a test; see `crates/niri-xmb-config-studio/locales/README.md`
for the convention and how a future translation plugs in.

## Checks

```sh
cargo test -p niri-xmb-config-studio
cargo clippy --all-targets -- -D warnings
```

The helper tests cover patch generation, unknown/comment preservation,
array-of-table patches, binding removal, atomic replacement, and a real
`config-check` + atomic save of the shipped example.
