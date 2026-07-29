# Noctalia v5 port

The v5 port is a real Noctalia plugin in `plugins/niri-xmb`. Each widget
instance independently targets one Niri output, resolves real application
icons through Noctalia, grows to its workspace/window contents, focuses on
click, cycles workspaces on the wheel, and routes tiled drag/drop through the
standalone XMB daemon.

The Noctalia fork adds only the generic host support the plugin API was missing:
bar-side declarative drag/drop, click-without-drag callbacks, and optional drop
coordinates. Window/workspace state and all Niri actions remain in `niri-xmb`.

The desktop profile uses native Noctalia taskbar widgets. Set this field on
each taskbar instance to replace middle-click window close with XMB toggle:

```toml
middle_click_command = "$HOME/.local/bin/niri-xmb toggle"
```

Leave the field empty to keep the native close-window action.

Use `config.toml.example` as the two-instance template. Keep this in the
standalone backend configuration:

```toml
bars_enabled = false
```

The daemon is also still responsible for the cursor-positioned `Mod+Z` popup.
Build both projects on the desktop; do not compile Noctalia on the laptop.

Validate the deployed v5 configuration before restarting:

```bash
noctalia config validate ~/.config/noctalia/config.toml
```
