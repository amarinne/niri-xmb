# Noctalia v5 plugin

This is the native Noctalia v5 bar port. It renders one independently targeted
Niri output per widget instance while keeping the popup and all window movement
logic in the standalone `niri-xmb` daemon.

The patched Noctalia build in this fork enables declarative drag/drop for bar
plugins, distinguishes click from drag, and passes drop coordinates to the
plugin. The plugin sends those coordinates back to `niri-xmb drop`, allowing
the daemon to restore the cursor after its temporary focus operations.

Example:

```toml
[plugins]
enabled = ["amarinne/niri-xmb"]

[[plugins.source]]
name = "niri-xmb-local"
kind = "path"
location = "/home/eza/Projects/niri-xmb/ports/noctalia-v5/plugins"

[bar.main]
start = ["niri-xmb-laptop"]

[widget.niri-xmb-laptop]
type = "amarinne/niri-xmb:workspace-strip"
target_output = "eDP-1"
capsule = false
backend_command = "/home/eza/.local/bin/niri-xmb"
allow_drag = true
allow_column_join = false
preserve_cursor_after_drag = true
hide_empty_workspaces = false
hide_when_output_missing = false
show_active_indicator = true
icon_size = 22
group_opacity = 0.95
poll_interval_ms = 250
```

Set `capsule = false` on the Noctalia widget instance. The plugin draws and
resizes one capsule per Niri workspace; a bar-level capsule would wrap the
entire strip.

Keep `bars_enabled = false` in the standalone XMB configuration. The daemon
must be running for snapshots, clicks, wheel switching, drops, and the
`Mod+Z` popup.
