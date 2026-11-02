#!/bin/sh
# Installs candeck and its desktop icon. Uses the files beside this script from a release
# archive or a built checkout. Run on its own, it downloads the latest release first.
set -eu

BASE_URL="${CANDECK_URL:-https://github.com/entitledosprey/canbus-deck/releases/latest/download}"

fail() {
    echo "install: $1" >&2
    exit 1
}

[ "$(uname -s)" = "Linux" ] || fail "candeck needs Linux, because it uses SocketCAN."

SOURCE=""
if [ -f "$0" ] && [ "$(basename "$0")" = "install.sh" ]; then
    HERE="$(cd "$(dirname "$0")" && pwd)"
    if [ -x "$HERE/candeck" ]; then
        BIN="$HERE/candeck"
        SOURCE="$HERE"
    elif [ -x "$HERE/../target/release/candeck" ]; then
        BIN="$HERE/../target/release/candeck"
        SOURCE="$HERE"
    fi
fi

TMP=""
cleanup() {
    if [ -n "$TMP" ]; then
        rm -rf "$TMP"
    fi
}
trap cleanup EXIT

if [ -z "$SOURCE" ]; then
    case "$(uname -m)" in
        aarch64 | arm64) ARCH=aarch64 ;;
        x86_64 | amd64) ARCH=x86_64 ;;
        *) fail "no download for $(uname -m). Build from source instead, using the README." ;;
    esac
    command -v curl >/dev/null || fail "curl is required."
    command -v sha256sum >/dev/null || fail "sha256sum is required."

    NAME="candeck-$ARCH-linux"
    TMP="$(mktemp -d)"
    echo "Downloading $NAME..."
    curl -fsSL "$BASE_URL/$NAME.tar.gz" -o "$TMP/$NAME.tar.gz" || fail "download failed: $BASE_URL/$NAME.tar.gz"
    curl -fsSL "$BASE_URL/$NAME.tar.gz.sha256" -o "$TMP/$NAME.tar.gz.sha256" || fail "checksum download failed."
    (cd "$TMP" && sha256sum --quiet -c "$NAME.tar.gz.sha256") || fail "checksum did not match, so nothing was installed."
    tar xzf "$TMP/$NAME.tar.gz" -C "$TMP"
    SOURCE="$TMP/$NAME"
    BIN="$SOURCE/candeck"
fi

sudo install -m 0755 -o root -g root "$BIN" /usr/local/bin/candeck
sudo install -m 0755 -o root -g root "$SOURCE/candeck-start" /usr/local/bin/candeck-start

install -D -m 0755 "$SOURCE/CAN Sniffer.desktop" "$HOME/Desktop/CAN Sniffer.desktop"
install -D -m 0644 "$SOURCE/CAN Sniffer.desktop" "$HOME/.local/share/applications/can-sniffer.desktop"

echo "Installed $(/usr/local/bin/candeck --version) and the CAN Sniffer desktop icon."
