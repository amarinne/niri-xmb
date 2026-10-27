# Config Studio

Native `egui` editor for the controller and XMB TOML.

## Run

```sh
cargo build -p niri-xmb --bin niri-xmb
cargo run -p niri-xmb-config-studio -- \
  --config ~/.config/niri-xmb/config.toml \
  --niri-xmb target/debug/niri-xmb
```

Use the installed `niri-xmb` path for an installed build. The window loads the
selected TOML immediately.

Install the helper, icon, and application entry:

```sh
cargo build --release -p niri-xmb-config-studio
scripts/install-config-studio.sh
```

This installs into `~/.local` by default; set `PREFIX=/path` to override. The
entry appears as **niri-xmb Config**.

## Behavior

- **Controller:** slim detected-controller status, the controller map with
  hotspots and the selected-binding inspector, then Device/Pointer/Bindings/
  Layers in a two-column grid.
- **Trigger Lab:** Response (graphs, presets, point inspector) beside Tune
  (pedals, speed multipliers, deadzone/curve/SOCD, pointer/scroll accel).
- **Popup:** panel, placements, behavior grouped into Input / Close when / Drop
  behavior, peek, and animation.
- **Appearance:** theme, colors, geometry, and bar configuration.
- **Preview:** the single live XMB/bar/model surface. It uses the daemon's live
  status/snapshot, not a static mock; clicking a window sends the existing
  `focus-window` command.
- The top command bar holds the config path, Load, and live daemon status. A
  persistent bottom action bar holds change count/SHA, Changes, Preview,
  Validate, Save, Save & Restart, and Revert. Changes opens the patch-path
  drawer and the validated TOML view.
- Controller hotspots select a control; the inspector edits the active Navi
  base, XMB, or layer binding. Both templates use the same display box.
- Trigger Lab's compact pedals use the same points and speed-SOCD policy; they
  are simulation, not live controller input. `speed_socd` selects `last`,
  `first`, `cancel`, `multiply`, `prio_decel`, or `prio_accel` for simultaneous
  speed inputs. This does not alter trigger bindings or layer hold_axes.
- Below 700 px the rail becomes a top strip; below 900/1000 px the controller,
  trigger, and appearance grids stack.

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

## Checks

```sh
cargo test -p niri-xmb-config-studio
cargo clippy --all-targets -- -D warnings
```

The helper tests cover patch generation, unknown/comment preservation,
array-of-table patches, binding removal, atomic replacement, and a real
`config-check` + atomic save of the shipped example.
