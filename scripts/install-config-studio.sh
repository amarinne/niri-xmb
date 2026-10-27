#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prefix=${PREFIX:-$HOME/.local}
binary=${BINARY:-$root/target/release/niri-xmb-config-studio}

if [ ! -x "$binary" ]; then
    echo "error: build niri-xmb-config-studio first: cargo build --release -p niri-xmb-config-studio" >&2
    exit 1
fi

install -Dm755 "$binary" "$prefix/bin/niri-xmb-config-studio"
install -Dm644 "$root/assets/niri-xmb-config-studio.svg" \
    "$prefix/share/icons/hicolor/scalable/apps/niri-xmb-config-studio.svg"
install -Dm644 "$root/niri-xmb-config-studio.desktop" \
    "$prefix/share/applications/niri-xmb-config-studio.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$prefix/share/applications" >/dev/null 2>&1 || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$prefix/share/icons/hicolor" >/dev/null 2>&1 || true
fi

echo "installed niri-xmb config studio helper in $prefix; launch it with: niri-xmb gui"
