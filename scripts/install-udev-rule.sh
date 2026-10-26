#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
rules_dir=${UDEV_RULES_DIR:-/etc/udev/rules.d}

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root, for example: sudo $0" >&2
    exit 1
fi

install -Dm644 "$root/udev/60-niri-xmb-gamepad.rules" \
    "$rules_dir/60-niri-xmb-gamepad.rules"
if command -v udevadm >/dev/null 2>&1; then
    udevadm control --reload-rules
    udevadm trigger --subsystem-match=input
fi
echo "Installed niri-xmb gamepad uaccess rule in $rules_dir"
