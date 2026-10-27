#!/bin/sh
set -eu

REPOSITORY="${NIRI_XMB_REPOSITORY:-amarinne/niri-xmb}"
VERSION="${NIRI_XMB_VERSION:-latest}"
INSTALL_DIR="${NIRI_XMB_INSTALL_DIR:-${HOME}/.local/bin}"
DATA_DIR="${XDG_DATA_HOME:-${HOME}/.local/share}"
CONFIG_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/niri-xmb"
SYSTEMD_DIR="${XDG_CONFIG_HOME:-${HOME}/.config}/systemd/user"
NIRI_CONFIG_PATH="${NIRI_CONFIG:-${XDG_CONFIG_HOME:-${HOME}/.config}/niri/config.kdl}"
NIRI_BLOCK_START='// >>> niri-xmb setup >>>'
NIRI_BLOCK_END='// <<< niri-xmb setup <<<'

usage() {
    cat <<'EOF'
Install, update, or remove niri-xmb.

Usage:
  setup.sh [OPTIONS]
  setup.sh install [OPTIONS]
  setup.sh update [OPTIONS]
  setup.sh uninstall [--install-dir DIR] [--purge]

Install options:
  --version TAG       Install a release tag instead of the latest release.
  --install-dir DIR   Install the binary in DIR.
  --init-config       Create config.toml from the full example.
  --no-niri-config    Do not add the managed Niri spawn-at-startup block.
  --no-service        Do not install the optional systemd user service.

Uninstall options:
  --install-dir DIR   Remove the binary from DIR.
  --purge             Also remove ~/.config/niri-xmb.

Environment:
  NIRI_XMB_REPOSITORY   GitHub repository (default: amarinne/niri-xmb)
  NIRI_XMB_VERSION      Release tag or "latest"
  NIRI_XMB_INSTALL_DIR  Binary directory (default: ~/.local/bin)
EOF
}

action=install
case "${1:-}" in
    install|uninstall)
        action=$1
        shift
        ;;
    update)
        action=install
        shift
        ;;
esac

install_service=true
manage_niri_config=true
init_config=false
purge=false
while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || { echo "error: --version needs a tag" >&2; exit 2; }
            VERSION=$2
            shift 2
            ;;
        --install-dir)
            [ "$#" -ge 2 ] || { echo "error: --install-dir needs a directory" >&2; exit 2; }
            INSTALL_DIR=$2
            shift 2
            ;;
        --no-service)
            install_service=false
            shift
            ;;
        --init-config)
            init_config=true
            shift
            ;;
        --no-niri-config)
            manage_niri_config=false
            shift
            ;;
        --purge)
            purge=true
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

remove_niri_block() {
    source_file=$1
    output_file=$2
    awk -v start="$NIRI_BLOCK_START" -v end="$NIRI_BLOCK_END" '
        $0 == start { managed = 1; next }
        $0 == end { managed = 0; next }
        !managed { print }
    ' "$source_file" >"$output_file"
}

remove_niri_config_entry() {
    [ -f "$NIRI_CONFIG_PATH" ] || return 0
    grep -Fq "$NIRI_BLOCK_START" "$NIRI_CONFIG_PATH" || return 0
    config_dir=$(dirname "$NIRI_CONFIG_PATH")
    candidate=$(mktemp "$config_dir/.niri-xmb-config.XXXXXX")
    remove_niri_block "$NIRI_CONFIG_PATH" "$candidate"
    chmod 0644 "$candidate"
    mv "$candidate" "$NIRI_CONFIG_PATH"
    echo "Removed managed Niri config block from $NIRI_CONFIG_PATH"
}

if [ "$action" = uninstall ]; then
    [ "$init_config" = false ] || { echo "error: --init-config only works with install" >&2; exit 2; }
    [ "$install_service" = true ] || { echo "error: --no-service only works with install" >&2; exit 2; }

    if [ -x "$INSTALL_DIR/niri-xmb" ]; then
        "$INSTALL_DIR/niri-xmb" quit >/dev/null 2>&1 || true
    fi
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now niri-xmb.service >/dev/null 2>&1 || true
    fi
    rm -f "$INSTALL_DIR/niri-xmb" "$INSTALL_DIR/niri-xmb-config-studio" \
        "$SYSTEMD_DIR/niri-xmb.service" \
        "$DATA_DIR/applications/niri-xmb-config-studio.desktop" \
        "$DATA_DIR/icons/hicolor/scalable/apps/niri-xmb-config-studio.svg"
    if [ "$manage_niri_config" = true ]; then
        remove_niri_config_entry
    fi
    if [ "$purge" = true ]; then
        rm -rf "$CONFIG_DIR"
        echo "Removed $CONFIG_DIR"
    fi
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
    echo "Uninstalled niri-xmb"
    [ "$purge" = true ] || echo "Kept user files in $CONFIG_DIR"
    exit 0
fi

[ "$purge" = false ] || { echo "error: --purge only works with uninstall" >&2; exit 2; }

for command in curl tar sha256sum install awk grep sed; do
    command -v "$command" >/dev/null 2>&1 || {
        echo "error: required command not found: $command" >&2
        exit 1
    }
done

case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64) arch=x86_64 ;;
    Linux:aarch64|Linux:arm64) arch=aarch64 ;;
    *)
        echo "error: unsupported platform: $(uname -s) $(uname -m)" >&2
        exit 1
        ;;
esac

archive="niri-xmb-linux-${arch}.tar.gz"
if [ "$VERSION" = latest ]; then
    release_url="https://github.com/${REPOSITORY}/releases/latest/download"
else
    release_url="https://github.com/${REPOSITORY}/releases/download/${VERSION}"
fi
release_url="${NIRI_XMB_RELEASE_URL:-$release_url}"

download() {
    output=$1
    url=$2
    if [ -n "${NIRI_XMB_RELEASE_URL:-}" ]; then
        curl --fail --location --silent --show-error --output "$output" "$url"
    else
        curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
            --output "$output" "$url"
    fi
}

tmp_dir=$(mktemp -d)
trap 'rm -rf "$tmp_dir"' EXIT HUP INT TERM

echo "Downloading ${archive}..."
download "$tmp_dir/$archive" "$release_url/$archive"
download "$tmp_dir/SHA256SUMS" "$release_url/SHA256SUMS"

expected=$(awk -v file="$archive" '$2 == file || $2 == "*" file { print $1; exit }' "$tmp_dir/SHA256SUMS")
[ -n "$expected" ] || {
    echo "error: checksum missing for $archive" >&2
    exit 1
}
actual=$(sha256sum "$tmp_dir/$archive" | awk '{ print $1 }')
[ "$actual" = "$expected" ] || {
    echo "error: checksum failed for $archive" >&2
    exit 1
}

mkdir -p "$tmp_dir/package"
tar -xzf "$tmp_dir/$archive" -C "$tmp_dir/package"
[ -x "$tmp_dir/package/niri-xmb" ] || {
    echo "error: release archive has no niri-xmb binary" >&2
    exit 1
}

install -d "$INSTALL_DIR" "$CONFIG_DIR/themes"
install_result=Installed
if [ -x "$INSTALL_DIR/niri-xmb" ]; then
    install_result=Updated
fi
binary_tmp=$(mktemp "$INSTALL_DIR/.niri-xmb.XXXXXX")
install -m 0755 "$tmp_dir/package/niri-xmb" "$binary_tmp"
mv "$binary_tmp" "$INSTALL_DIR/niri-xmb"

if [ -x "$tmp_dir/package/bin/niri-xmb-config-studio" ]; then
    install -d "$DATA_DIR/applications" \
        "$DATA_DIR/icons/hicolor/scalable/apps"
    studio_tmp=$(mktemp "$INSTALL_DIR/.niri-xmb-config-studio.XXXXXX")
    install -m 0755 "$tmp_dir/package/bin/niri-xmb-config-studio" "$studio_tmp"
    mv "$studio_tmp" "$INSTALL_DIR/niri-xmb-config-studio"
    install -m 0644 "$tmp_dir/package/share/applications/niri-xmb-config-studio.desktop" \
        "$DATA_DIR/applications/niri-xmb-config-studio.desktop"
    install -m 0644 "$tmp_dir/package/share/icons/hicolor/scalable/apps/niri-xmb-config-studio.svg" \
        "$DATA_DIR/icons/hicolor/scalable/apps/niri-xmb-config-studio.svg"
    echo "${install_result} config studio helper (launch with: niri-xmb gui)"
fi

if [ ! -e "$CONFIG_DIR/config.toml.example" ]; then
    install -m 0644 "$tmp_dir/package/config/niri-xmb.toml.example" \
        "$CONFIG_DIR/config.toml.example"
fi
if [ "$init_config" = true ]; then
    if [ -e "$CONFIG_DIR/config.toml" ]; then
        echo "Kept existing config: $CONFIG_DIR/config.toml"
    else
        install -m 0644 "$tmp_dir/package/config/niri-xmb.toml.example" \
            "$CONFIG_DIR/config.toml"
        echo "Created $CONFIG_DIR/config.toml"
    fi
fi
for theme in "$tmp_dir/package/themes/"*.toml; do
    name=$(basename "$theme")
    if [ ! -e "$CONFIG_DIR/themes/$name" ]; then
        install -m 0644 "$theme" "$CONFIG_DIR/themes/$name"
    fi
done

if [ "$install_service" = true ]; then
    install -d "$SYSTEMD_DIR"
    awk -v binary="$INSTALL_DIR/niri-xmb" \
        '/^ExecStart=/{ print "ExecStart=" binary " daemon"; next } { print }' \
        "$tmp_dir/package/niri-xmb.service" >"$tmp_dir/niri-xmb.service"
    install -m 0644 "$tmp_dir/niri-xmb.service" "$SYSTEMD_DIR/niri-xmb.service"
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload >/dev/null 2>&1 || true
    fi
fi

if [ "$manage_niri_config" = true ]; then
    niri_config_dir=$(dirname "$NIRI_CONFIG_PATH")
    install -d "$niri_config_dir"
    [ -e "$NIRI_CONFIG_PATH" ] || : >"$NIRI_CONFIG_PATH"

    if grep -Fq "$NIRI_BLOCK_START" "$NIRI_CONFIG_PATH"; then
        base_config="$tmp_dir/niri-config-base.kdl"
        remove_niri_block "$NIRI_CONFIG_PATH" "$base_config"
    elif grep -ERq --include='*.kdl' \
        '^[[:space:]]*spawn-at-startup[[:space:]].*niri-xmb.*daemon' \
        "$niri_config_dir"; then
        base_config=
        echo "Kept existing niri-xmb spawn-at-startup line in $NIRI_CONFIG_PATH"
    else
        base_config="$NIRI_CONFIG_PATH"
    fi

    if [ -n "$base_config" ]; then
        escaped_binary=$(printf '%s' "$INSTALL_DIR/niri-xmb" | sed 's/\\/\\\\/g; s/"/\\"/g')
        candidate=$(mktemp "$niri_config_dir/.niri-xmb-config.XXXXXX")
        awk '1; END { if (NR && length($0)) print "" }' "$base_config" >"$candidate"
        {
            echo "$NIRI_BLOCK_START"
            echo "spawn-at-startup \"$escaped_binary\" \"daemon\""
            echo "$NIRI_BLOCK_END"
        } >>"$candidate"
        if command -v niri >/dev/null 2>&1 && ! niri validate --config "$candidate"; then
            rm -f "$candidate"
            echo "error: Niri rejected generated config; original file kept" >&2
            exit 1
        fi
        chmod 0644 "$candidate"
        mv "$candidate" "$NIRI_CONFIG_PATH"
        echo "Updated $NIRI_CONFIG_PATH"
    fi
fi

echo "$install_result niri-xmb at $INSTALL_DIR/niri-xmb"
echo "Default settings auto-detect connected Niri outputs."
if [ "$manage_niri_config" = true ]; then
    echo "Reload Niri to start niri-xmb."
else
    echo "Add this line to your Niri config:"
    echo "  spawn-at-startup \"$INSTALL_DIR/niri-xmb\" \"daemon\""
fi
echo "Optional config template: $CONFIG_DIR/config.toml.example"
