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

- Controller tab: device/preset/autodetect, device paths, open/mode combos,
  pointer/scroll, trigger acceleration, XMB buttons, navi bindings, layers.
- XMB tab: panel outputs, surface/position/size, per-output placements,
  behavior/outside-click, border drag, peek, animation.
- Style tab: theme path, colors, borders, shadows, icon/panel geometry.
- Bars tab: current `BarConfig` fields, including wheel behavior.
- Preview tab: controller-adjacent XMB and bar previews drawn from the same
  model. This is a geometry/style preview, not a renderer pixel match.
- Controller hotspots select a control; the inspector edits the active Navi
  base, XMB, or layer binding. A **Detected controller** card shows the daemon's
  live mode, preset, and connected device names/paths. Both controller templates
  use the same display box.
- **Preview** uses the daemon's live status/snapshot, not a static mock. It draws
  current workspaces/windows and selection; clicking a window sends the existing
  `focus-window` command. The pane is toggleable and collapsed by default.
- Trigger Lab contains the shared piecewise-linear response curve: add/drag/
  delete up to five points, edit exact pull/shape values, choose a preset or
  convert the legacy curve, and inspect the resulting LT/RT multiplier and
  percent. The compact pedals use the same points; they are simulation, not
  live controller input.

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
