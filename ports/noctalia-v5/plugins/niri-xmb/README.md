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
location = "/path/to/niri-xmb/ports/noctalia-v5/plugins"

[bar.main]
start = ["niri-xmb-main"]

[widget.niri-xmb-main]
type = "amarinne/niri-xmb:workspace-strip"
target_output = "eDP-1"
capsule = false
backend_command = "niri-xmb"
allow_drag = true
allow_column_join = false
preserve_cursor_after_drag = true
hide_empty_workspaces = false
hide_when_output_missing = false
active_indicator_mode = "dot"
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

## Application icons

The plugin sends several identity candidates to Noctalia's application icon
resolver. It checks explicit desktop metadata, the Niri application ID,
common terminal-class suffixes, and stable icon-like window titles. This lets
a class such as `btop-terminal` resolve the `btop.desktop` icon.

Noctalia remains responsible for desktop-entry matching and icon-theme lookup.

## State transport

When the Noctalia runtime provides `runStream()`, the widget starts one
`niri-xmb subscribe` child and consumes the daemon's NDJSON state stream. The
subscription is event-driven: a healthy stream starts no `snapshot` or `status`
process during normal updates.

- Stream frames are decoded as they arrive but applied at most once per update
  tick. Repeated generations collapse to the newest complete frame, and an
  older generation or `content_revision` never replaces a newer one.
- A title-only update keeps `generation` and bumps `content_revision`; it is
  delivered and rendered.
- Pointer, press/release, drag-threshold, and keyboard callbacks stay
  immediate. Only state application and icon work wait for the tick.
- The stream command appends a private `stream_end` frame so an exited
  subscription triggers a bounded relaunch with backoff. Three launches per
  entry runtime is the cap.
- If `runStream()` is absent, rejected, or the stream sends a non-retryable
  error, the widget falls back to the previous mirror/process polling path.
  `poll_interval_ms` controls that fallback, not the healthy stream.
- Noctalia drops a single `runStream` line above 64 KiB while the daemon allows
  256 KiB frames. If the initial snapshot is oversized, polling remains
  active. A healthy stream also re-reads the mirror every 30 seconds as a
  safety net for a dropped frame or a silently dead child.

Noctalia has no stop handle for `runStream()` at this API level, so one child
is left per widget instance until the entry reloads. The daemon bounds active
subscribers at eight; keep widget instances below that. A `[[service]]` bridge
would share one child across outputs, but it needs a manifest change.

## Checks

The deterministic harness mocks the Noctalia host and drives stream framing,
batching, fallback polling, focus/cycle intents, and drop callbacks:

```sh
luajit ports/noctalia-v5/plugins/niri-xmb/tests/run.lua
```

A live session is still required to verify actual Noctalia delivery and
duplicate-input behavior.
