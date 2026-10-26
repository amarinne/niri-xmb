# niri-xmb UX specification

This document defines the product interaction rules. See
[MACHINE_PROFILE.md](MACHINE_PROFILE.md) for one machine-specific configuration.
The words **must**, **should**, and **may** state requirement levels.

## Design goals

- The popup must show Niri state without changing the window layout.
- Window organization must not move pointer focus to the target output.
- Surfaces outside the panel must keep normal pointer and click behavior.
- Window activation from the panel must require one action.
- Multi-window organization must keep the popup open when configured.
- Missing optional configuration must use documented defaults.
- Invalid configuration must not stop the active daemon.

## Runtime modes

| Mode | Trigger | Pointer input | Keyboard input | Close rule |
| --- | --- | --- | --- | --- |
| Hidden | Startup or explicit hide | None | None | Not applicable |
| Manual | `show` or `toggle` | Panel and capture surface | Yes | Outside click always closes |
| Peek | Niri workspace navigation | None, or panel with pointer promotion | None | Hide after `peek.duration_ms` |
| Peek armed | Pointer moves onto a promoted peek panel | Panel only | None | Hide after configured pointer exit delay |

Manual mode must take keyboard focus for navigation. Peek mode must remain passive.
The daemon must track pointer focus and keyboard focus separately.
A manual panel must keep answering keys after the pointer leaves the panel.
Opening manual mode while peek is visible must promote the existing popup.
The configured event policy controls which Niri events raise peek.
Each qualifying focus move must extend active peek without hiding it first.
Peek expiry must pause while a focus event burst settles.

## Peek promotion

`peek.pointer_promote = false` keeps the overlay click-through. Promotion is then
keybind-only. All pointer events pass to the surface below.

`peek.pointer_promote = true` enables pointer handover:

- The overlay must stay click-through for `peek.arm_grace_ms` after it appears.
- The panel rectangle must accept pointer input after the grace period.
- No surface outside the panel may accept pointer input for the overlay.
- Peek must never take keyboard focus.
- Pointer motion over the panel must arm the overlay.
- A stationary pointer must not arm the overlay.
- An armed overlay must not expire while the pointer rests on it.
- An armed overlay must use `style.accent` after it claims pointer input.
- Pointer exit must hide the overlay after `peek.demote_ms`.
- A click on peek must promote it and perform the selected action once.
- Promotion must reuse the mapped surface without a second open animation.
- Peek must never capture the full output.

## Panel placement and outputs

`popup.outputs` controls which output maps appear in a popup.
`popup.surface_mode` controls whether the popup stays on its initial output.
`popup.position` and placement rules control the popup anchor and margin.

The daemon must add a popup surface when an output connects after startup.
An absent output must not create visible empty content.
`popup.surface_mode = "fixed"` must keep the panel on its initial output.
`popup.surface_mode = "follow_active"` must follow focus after `follow_delay_ms`.

Workspaces must run from top to bottom in each output map.
Windows must follow Niri column order from left to right.
Panel size must shrink to visible content and stop at configured maximum sizes.

## Pointer behavior

Manual mode must advertise only required input regions.
Armed peek mode must advertise only the visible panel as its input region.
Windows, bars, and docks outside those regions must receive normal pointer input.
Dock magnification and drag operations must work while the popup is open.

`close_on_external_focus_change` controls external focus handling.
Manual mode always captures and closes on an outside click. The legacy
`close_on_outside_click` setting is deprecated and ignored. `replay_outside_click`
independently controls whether the daemon replays that click after dismissal.
Click replay cannot reproduce hover or drag motion.

An outside press does not close while a press begun inside the panel is active.
The release resolves the click or drag first; a completed drag follows the
drop closure setting.

## Popup lifetime

Behavior settings must independently control closure after these actions:

- Focus a window.
- Focus a workspace.
- Complete drag and drop.
- Click outside the popup.
- Press `Escape`.
- Run the toggle command.

An action that keeps the popup open must preserve valid navigation state.
An action that closes the popup must release all input focus and capture surfaces.

## Keyboard navigation

In `navigation = "deferred"` mode, arrow keys must change only popup selection.
Enter must apply the selected window or workspace action.
Selection must remember its position on each visited workspace during one session.
Closing the popup must clear staged selection.

In `navigation = "live"` mode, each navigation step may move Niri focus.
XMB-generated focus changes must not count as external changes.
External Niri keybinds must not change manual mode into hidden or peek mode.

## Drag and drop

Pointer travel must exceed `drag_threshold` before a press becomes a drag.
Drop behavior settings must control tiled, floating, focus, and cursor results.
The result must match those settings for empty workspaces and window targets.

When `behavior.move_popup` is enabled, pressing the configured popup border and
moving beyond `behavior.drag_threshold` moves the panel for that session.
`peek.move_popup` separately opts an armed peek into this movement; passive peek
remains click-through. A border press never dispatches a Niri focus action.

When column joining is disabled, a window target must have two zones.
The left zone inserts before the target. The right zone inserts after the target.
No target zone may create a joined Niri column in this mode.

## Noctalia taskbar integration

Each taskbar widget must target one Niri output.
Each widget must route focus, workspace cycling, and drag through `niri-xmb`.
Taskbar focus changes must not raise peek unless the event policy requests it.
Wheel input must start from the workspace row under the pointer.
A wheel gesture must keep a stable target until the pointer changes rows.
Queued requests must use the previous queued target until Niri updates state.
Ratchet input must move one workspace per complete 120-unit step.
Free-spin input must use the configured repeat limit.

A configured middle-click command must replace native window close.
An omitted or empty command must keep native window close.
Middle-click behavior must apply to grouped and running pinned icons.
An output-specific widget must remain empty when its output is absent.

Noctalia dock reordering must apply only to pinned applications.
The configured hold delay must expire before a pinned icon moves.

## Urgency indicators

Urgency means that a Niri window requests attention.
It does not mean normal focus or selection.

`style.urgent_indicator` supports these presets:

| Preset | Result |
| --- | --- |
| `"none"` | Hide window urgency indicator |
| `"ring"` | Draw an outline around the icon |
| `"glow"` | Draw a soft radial background glow |
| `"badge"` | Draw a dot at the icon top-right corner |

The theme field `style.urgent` must control indicator color.
The indicator must remain visible in Niri overview mode.
The indicator must follow Niri urgency changes without a daemon restart.

Use these commands to test urgency:

```bash
niri msg windows
niri msg action set-window-urgent --id WINDOW_ID
niri msg action unset-window-urgent --id WINDOW_ID
```

Niri does not set urgency on the focused window. Select a non-focused window ID.

## Visual treatment

Manual panels must use `style.accent` while XMB owns keyboard input.
Armed peek panels must use `style.accent` after they claim pointer input.
Normal peek panels must use `style.border`.
The fixed focal icon, scale, and opacity must indicate selection.

When `popup.show_titles` is true, a vertical stack must show titles on the right.
Each title must fit inside the canvas.
The below-icon title position applies only outside vertical stacks.
Transparency must not change pointer input regions or popup lifetime behavior.

## Configuration resolution

An omitted field must use its Rust `Default` value.
An omitted section must use all defaults for that section.
A missing configuration file must use the complete built-in configuration.

An empty value is not an omitted value.
Invalid TOML, a wrong type, or an unknown preset must return an error.
`niri-xmb restart` must validate configuration before it stops the active daemon.
The active daemon must keep its current configuration after validation failure.

A referenced theme may contain a partial `[style]` table.
Theme fields must override matching fields from the main configuration.
Absent theme fields must keep main configuration or built-in values.
