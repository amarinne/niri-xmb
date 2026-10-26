# Vader 4 DInput fixtures

Synthetic 32-byte protocol fixtures for the scoped DInput parser. They are
**not hardware captures** and do not prove physical button labels or sensor
axes; see `DINPUT_SPIKE.md` for the unresolved checks.

- `vader4-dinput-neutral.bin` — report 0x04, centered sticks, no presses.
- `vader4-dinput-pressed.bin` — all mapped buttons, diagonal D-pad, stick
  extremes, analog triggers, and non-zero gyro/accel channels.

Run:

```sh
cargo run --bin vader4-dinput-probe -- tests/fixtures/vader4-dinput-pressed.bin
```

Source map/license ledger: see [DINPUT_SPIKE.md](../../../../DINPUT_SPIKE.md).
