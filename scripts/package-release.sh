#!/bin/sh
set -eu

target=${1:?usage: package-release.sh TARGET ARCH [OUTPUT_DIR]}
arch=${2:?usage: package-release.sh TARGET ARCH [OUTPUT_DIR]}
output_dir=${3:-dist}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT HUP INT TERM

cd "$root"
cargo build --locked --release --target "$target" -p niri-xmb

install -Dm755 "target/$target/release/niri-xmb" "$stage/niri-xmb"
install -Dm644 config/niri-xmb.toml.example \
    "$stage/config/niri-xmb.toml.example"
install -Dm644 themes/dank.toml "$stage/themes/dank.toml"
install -Dm644 themes/noctalia.toml "$stage/themes/noctalia.toml"
install -Dm644 themes/material-3-expressive.toml "$stage/themes/material-3-expressive.toml"
install -Dm644 niri-xmb.service "$stage/niri-xmb.service"
install -Dm644 LICENSE "$stage/LICENSE"

mkdir -p "$output_dir"
tar -C "$stage" -czf "$output_dir/niri-xmb-linux-${arch}.tar.gz" .

