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

## Gamepad input

Optional evdev gamepads start in **game** mode. The daemon reads only configured
absolute device paths; an empty list discovers
`/dev/input/by-id/*-event-joystick` and `*-event-gamepad`. It does not grab the
device in game mode, so games receive normal input. The mode toggle
(`gamepad.mode.toggle`, default `BTN_MODE`+`BTN_START`) switches to **navi**
mode, which grabs the device and activates desktop navigation. The completing
toggle press is consumed; a device that disconnects resets the mode to game.

Navi mode maps stick motion, clicks, and scroll through the Wayland virtual
pointer. The default base bindings are left stick pointer, right stick scroll,
South/Cross left click, East/Circle right click, West/Square middle click,
North/Triangle open XMB, and Start toggle the Niri overview. Hold layers are
configured under `gamepad.navi.layers`: by default TL focuses workspaces and
windows, TR moves windows/columns, and holding both analog triggers focuses or
moves between monitors. D-pad hats and `BTN_DPAD_*` events both resolve these
layers. Navi actions call Niri directly, so workspace/output changes are
immediate rather than deferred.

Analog triggers modulate pointer and scroll speed around the configured baseline
speed. LT is a precision/deceleration control and RT is a boost. Trigger depth is
deadzoned and shaped, then combined multiplicatively with separate deceleration
floors, boost caps, and smoothing for pointer and scroll. The shape is either
the legacy power curve or an optional ordered list of up to five piecewise-linear
`trigger_points`; empty points preserve the legacy behavior. `speed_socd`
controls simultaneous LT/RT speed modulation: `last` (most recent press),
`first` (earliest held), `cancel` (neither), `multiply` (both factors),
`prio_decel` (brake wins), or `prio_accel` (gas wins). Trigger bindings and layer
hold_axes still see both raw triggers. The speed multiplier
is smoothed, not the stick position, so trigger ADC noise does not step the
pointer. Both triggers also participate in the monitor layer's hold_axes; layer
actions take precedence when the D-pad is used.

While the manual XMB is open, the existing `gamepad.buttons` mapping owns the
face/D-pad context; navi pointer motion remains available but pointer clicks and
Niri actions are suppressed. When the XMB closes, navi base/layers resume.
`gamepad.open_combo` is active only in navi mode. Presses trigger one action;
releases and repeats are ignored except for pointer holds and mode/layer
releases. Gamepad input is disabled by default.

The current xpad interface exposes standard Xbox controls only. Full Vader 4
paddles and gyro are a follow-up DInput/vendor-HID source; they are not inferred
from the standard axes.

`gamepad.preset` selects `none`, `auto`, `xbox`, or `ds4`. Explicit
`gamepad.buttons` entries override preset entries by evdev code. `auto` maps
Flydigi VADER4 and other xpad-compatible pads with face buttons to `xbox`, and
Sony/DualShock/DualSense devices to `ds4`; it falls back to explicit bindings
for devices without a recognized layout. Both presets map the D-pad to
navigation, South/Cross to confirm, East/Circle to close, West/Square to back,
North/Triangle to tab, TL/Select to back, TR/Start to confirm, and Mode to
close. D-pad `ABS_HAT0X`/`ABS_HAT0Y` axes are translated to the configured
`BTN_DPAD_*` actions, with neutral values acting as releases and no repeat
while a direction is held. A device that reports both hat axes and D-pad key
events produces one action per direction.

`gamepad.open_combo` lists at least two unique evdev button names or 16-bit
codes. Holding them simultaneously on one device emits one `open` action,
which shows and promotes the manual popup from hidden or peek, using the same
path as the `show` control command, and does nothing when the manual popup is
already open. The press that completes the combo emits `open` instead of that
button's configured action; further repeats are suppressed until one combo
button is released. An empty list disables the combo.

The daemon rescans `/dev/input/by-id` every 500 ms, starts a reader when a
controller appears, and stops/joins readers when it disappears. Configured
paths that are temporarily absent retry with a two-second backoff, so no
daemon restart is needed after connecting a controller. Reading the event
node still requires permission. The project-owned
`udev/60-niri-xmb-gamepad.rules` tags joystick devices with `uaccess`, giving
the active seat user an ACL without the broad `input` group; niri-xmb still
owns discovery, reading, decoding, hotplug, and action handling.

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
Wheel input goes to the panel under the pointer. When no panel is under the
pointer, it goes to the open panel on the focused output; a fixed panel keeps
its own output. With no open panel, it goes to the bar on the focused output.
Keyboard focus must not choose the wheel target.
When the pointer is over a bar, wheel input must start from the workspace row
under the pointer.
A wheel gesture must keep a stable target until the pointer changes rows or panels.
Queued requests must use the previous queued target until Niri updates state.
Ratchet input must move one workspace per complete 120-unit step.
Free-spin input must use the configured repeat limit.
High-resolution packets must accumulate across frames and preserve every complete
step in a packet, including multiple steps from one burst. Vertical and
horizontal axes must keep independent remainders. A packet that includes both
deprecated discrete data and value120 data must produce one logical report.
Touchpad and continuous sources accumulate 15 axis units per workspace step and
emit at most one step per frame. A direction change or axis stop clears that
partial accumulation. The high-resolution wheel remainder survives axis stop so
a detent split across frames still completes exactly once.

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
