# Vader 4 DInput Spike

Status: parser/fixture spike only. This does **not** change the live gamepad
path, start a HID reader, create a virtual gamepad, or enable DInput. No
deployment.

## Decision

Keep niri-xmb as the policy/UI owner. Adapt only the small, permissive report
decoder needed for the Flydigi extended DInput report instead of forking
InputPlumber or padctl.

The full DInput phase still needs a transport + hotplug source and a virtual
gamepad for game-mode passthrough. That boundary is larger than the parser; do
not make DInput a default path before it exists. If the virtual-gamepad
integration becomes too invasive, evaluate InputPlumber as a backend at that
boundary rather than forking it.

## What was added

- `crates/niri-xmb/src/dinput.rs` — 32-byte report parser, decoded button
  state, D-pad, sticks, triggers, gyro, and accelerometer channels.
- `crates/niri-xmb/src/bin/vader4-dinput-probe.rs` — reads one fixture or
  hidraw node and prints JSON. It does not touch the daemon.
- `crates/niri-xmb/tests/fixtures/vader4-dinput-{neutral,pressed}.bin` —
  synthetic protocol fixtures, not hardware captures.

## Source ledger

| Source | Commit/path | License | Use |
| --- | --- | --- | --- |
| [InputPlumber](https://github.com/ShadowBlip/InputPlumber) | `4ed1f50c7801e4f91fd17ce1a7b2b9d8d8b82e69`, `src/drivers/flydigi_vader_4_pro/hid_report.rs`, `driver.rs`, `event.rs`, `mod.rs` | GPL-3.0-or-later | Adapted the report length/ID, button bit positions, stick/trigger offsets, gyro assembly, and sensor scale constants. Runtime, hidapi, DBus, packed-struct, and target code were not copied. |
| [Vader4ProReader](https://github.com/dantmnf/Vader4ProReader) | `1120ed54d2f9caa77c0caae3207e6b26cd9cfbbf`, `Device/Vader4ProReport.cs` | Unlicense | Cross-check of the byte map and paddles. No C# runtime copied. |
| [padctl Vader 4 DInput config](https://github.com/BANANASJIM/padctl/blob/049953d0e5588266333af3a91e8e783d82cdd544/devices/flydigi/vader4-pro-04b4-2412.toml) | `049953d0e5588266333af3a91e8e783d82cdd544`, `devices/flydigi/vader4-pro-04b4-2412.toml` | LGPL-2.1-or-later | Device/interface IDs, report size, extended field offsets, and the conceptual output layout. No Zig code copied. |

Clean-room boundary: the parser is a manual Rust implementation from the
documented offsets. No proprietary code, assets, C#/Zig runtime, or UI was
copied. The exact upstream commits are pinned above for review.

## Validation

Focused tests:

```text
cargo test --bin vader4-dinput-probe
running 3 tests
... ok
```

Fixture probe:

```text
cargo run --quiet --bin vader4-dinput-probe -- \
  crates/niri-xmb/tests/fixtures/vader4-dinput-pressed.bin
```

The pressed fixture decodes all face/shoulder/stick/trigger/digital buttons,
guides/FN, M1–M4, C/Z, a diagonal D-pad, both stick extremes, trigger values
128/255, and non-zero gyro/accelerometer channels.

## Hardware validation gap

At the time of this spike the controller is in XInput/xpad mode:

```text
Bus 001 Device 064: ID 045e:028e Microsoft Corp. Xbox360 Controller
```

No `/sys/class/hidraw/*` node matches `04b4:2412`. The DInput report is exposed
only after switching the controller to DInput (`FN+A`, blue LED; vendor
interface 2). Hardware validation is therefore **not claimed**.

After switching, validate with:

```sh
lsusb | grep -i 04b4:2412
cargo run --bin vader4-dinput-probe -- /dev/hidrawN
```

## Unresolved mapping checks

These are deliberately called out because the sources do not fully agree:

1. **X/Y bits.** InputPlumber and padctl use X = byte 10 bit 0, Y = byte 9
   bit 7. Vader4ProReader labels those bytes in the opposite order. The spike
   follows InputPlumber/padctl; confirm against physical labels on hardware.
2. **Accelerometer Y/Z.** InputPlumber reads Y at bytes 13–14 and Z at 15–16;
   Vader4ProReader comments say Z at 13–14 and Y at 15–16. The spike follows
   InputPlumber.
3. **Paddles.** M1–M4 use byte 7 bits 2–5 and C/Z use bits 0–1, following
   Vader4ProReader. InputPlumber names some of these L4/R4/L5/R5; confirm the
   physical back-button mapping.
4. **D-pad diagonals** are the low nibble of byte 9 (1/2/4/8 up/right/down/left).
5. **Gyro Y** is assembled from bytes 18 (low) and 20 (high).

## Next slice

1. Add an explicit `[gamepad.dinput]` source boundary and HID hotplug.
2. Add a uinput virtual gamepad so game mode still works while the controller is
   in DInput.
3. Re-run the probe and the existing navi/trigger/layer tests against the real
   device before enabling DInput by default.
