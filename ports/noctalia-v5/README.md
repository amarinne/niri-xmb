# Noctalia v5 port

Noctalia v5 has a native C++ taskbar widget that already provides the bar-side
rendering niri-xmb needs: real application icons, Niri workspace grouping,
per-output filtering, adaptive width, workspace capsules, active indicators,
click-to-focus, and wheel workspace switching. The v5 port therefore uses a
named `taskbar` instance instead of adding a second layer-shell surface.

Copy the targeted widget tables from `config.toml.example` into the active
Noctalia v5 configuration. `target_output` decouples the displayed workspace
set from the monitor hosting the bar, so `niri-xmb-hdmi1` and
`niri-xmb-hdmi2` can sit next to each other on one bar. Their order in the
`start`, `center`, or `end` array determines left-to-right placement. If an
older v5 snapshot still uses a single `[bar]` table, keep that table and copy
the widget tables plus both array entries.

Keep this in the standalone backend configuration:

```toml
bars_enabled = false
```

The Rust daemon is still required for the `Mod+Z` organizer. Noctalia's native
taskbar currently handles click and wheel interaction but does not expose Niri
window rearrangement through its bar items. Use the popup for drag/drop on v5;
the v4 QML port additionally routes bar-item drops to `niri-xmb drop`.

Validate a current v5 configuration before restarting:

```bash
noctalia config validate ~/.config/noctalia/config.toml
```
