# Display texts and translations

## Why this shape

egui has no built-in internationalisation. The established Rust options are:

| Approach | Format | Notes |
| --- | --- | --- |
| Mozilla Fluent (`i18n-embed`, `fluent-templates`, `egui-i18n` with `fluent`) | `.ftl` | The modern Rust standard: plurals, gender, locale fallback. Adds a runtime and dependencies. |
| gettext (`gettext-rs`, `i18n-embed` gettext system) | `.po`/`.mo` | Mature translator tooling, awkward with Rust format strings. |
| Key-value files (`egui-i18n` classic, custom JSON/TOML) | `.egl`, `.json`, `.toml` | Simple lookup, no plural rules. Matches this repo's existing Noctalia plugin `translations/en.json` (`{label, description}` per key). |

No translation exists yet, so this app uses the third option now and keeps
the door open to the first: every string has a **stable key**, and keys do
not change when the English wording does. Moving to Fluent later is a
mechanical key-by-key copy into `.ftl` files.

## Files

- `en.json` — the English template. **Generated, do not edit by hand.**
  It has `{ "locale", "fallback", "messages": { "<key>": { "label",
  "description" } } }`, sorted by key.
- `src/i18n.rs` — `CATALOG`, the English source of truth in code. Field
  keys are config paths (`gamepad.navi.pointer.deadzone`); dynamic
  segments are written `*` (`bars.*.output`). Everything else is under
  `ui.` (`ui.action.save`, `ui.card.pointer`).
- This file — the convention.

The on-screen label and hover text are read from `CATALOG` at runtime
(field helpers resolve by config path), so the UI and the template cannot
drift. `description` doubles as the hover text in the UI and as the
translator's context note: one sentence saying what the thing does, its
unit or range, and what raising or lowering it does when that is not
obvious.

## Adding or changing a string

1. Add or edit the entry in `CATALOG` in `src/i18n.rs`. New config fields
   are covered automatically if the key equals the config path — the
   `catalog_covers_field_paths_used_in_main_rs` test scans `main.rs` and
   fails if a field path has no entry.
2. Regenerate the template:

   ```sh
   UPDATE_LOCALES=1 cargo test -p niri-xmb-config-studio i18n
   # or inspect without writing:
   cargo run -p niri-xmb-config-studio -- --print-strings
   ```

3. Run `cargo test -p niri-xmb-config-studio`. The
   `locales_en_json_matches_catalog` test fails if `en.json` is stale.

## Translating later

1. Copy `en.json` to `<lang>.json` (e.g. `de.json`, BCP 47 tag).
2. Translate `label` and `description` values. Keep keys, `{placeholders}`,
   units, and button codes such as `BTN_SOUTH` unchanged.
3. Runtime loading is not wired yet (English is compiled in). When a first
   translation lands, load `locales/<lang>.json` at startup, look up by
   key, and fall back to the compiled English entry — the catalog's
   lookup functions in `i18n.rs` are the single place to change.
