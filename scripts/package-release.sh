#!/bin/sh
set -eu

target=${1:?usage: package-release.sh TARGET ARCH [OUTPUT_DIR]}
arch=${2:?usage: package-release.sh TARGET ARCH [OUTPUT_DIR]}
output_dir=${3:-dist}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT HUP INT TERM

cd "$root"
cargo build --locked --release --target "$target" \
    -p niri-xmb -p niri-xmb-config-studio

install -Dm755 "target/$target/release/niri-xmb" "$stage/niri-xmb"
install -Dm755 "target/$target/release/niri-xmb-config-studio" \
    "$stage/bin/niri-xmb-config-studio"
install -Dm644 assets/niri-xmb-config-studio.svg \
    "$stage/share/icons/hicolor/scalable/apps/niri-xmb-config-studio.svg"
install -Dm644 niri-xmb-config-studio.desktop \
    "$stage/share/applications/niri-xmb-config-studio.desktop"
install -Dm644 config/niri-xmb.toml.example \
    "$stage/config/niri-xmb.toml.example"
install -Dm644 themes/dank.toml "$stage/themes/dank.toml"
install -Dm644 themes/noctalia.toml "$stage/themes/noctalia.toml"
install -Dm644 themes/material-3-expressive.toml "$stage/themes/material-3-expressive.toml"
install -Dm644 themes/liquid-glass.toml "$stage/themes/liquid-glass.toml"
install -Dm644 niri-xmb.service "$stage/niri-xmb.service"
install -Dm644 udev/60-niri-xmb-gamepad.rules \
    "$stage/udev/60-niri-xmb-gamepad.rules"
install -Dm755 scripts/install-udev-rule.sh \
    "$stage/scripts/install-udev-rule.sh"
install -Dm644 LICENSE "$stage/LICENSE"

mkdir -p "$output_dir"
tar -C "$stage" -czf "$output_dir/niri-xmb-linux-${arch}.tar.gz" .

