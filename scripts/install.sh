#!/usr/bin/env sh
set -eu

REPO="lucasliet/herdr-core-tui"
BIN_DIR="${HERDR_INSTALL_DIR:-$HOME/.local/bin}"
TAG=""

while [ "$#" -gt 0 ]; do
    case "$1" in
        --tag)
            shift
            [ "$#" -gt 0 ] || { echo "error: --tag requires a value" >&2; exit 2; }
            TAG="$1"
            ;;
        --prefix|--dir)
            shift
            [ "$#" -gt 0 ] || { echo "error: $1 requires a value" >&2; exit 2; }
            BIN_DIR="$1"
            ;;
        -h|--help)
            cat <<'EOF'
Usage: install.sh [--tag <tag>] [--prefix <dir>]

Installs herdr-core-tui and its hc-tui alias from GitHub Releases
(lucasliet/herdr-core-tui) into ~/.local/bin (or $HERDR_INSTALL_DIR / --prefix).
Existing `hc` / `herdr-app` binaries are never modified.
EOF
            exit 0
            ;;
        *)
            echo "unknown option: $1" >&2
            exit 2
            ;;
    esac
    shift
done

OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)
case "$OS" in
    linux) OS="linux" ;;
    darwin) OS="macos" ;;
    *) echo "error: unsupported OS: $OS" >&2; exit 1 ;;
esac
case "$ARCH" in
    x86_64|amd64) ARCH="x86_64" ;;
    aarch64|arm64) ARCH="aarch64" ;;
    *) echo "error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

ASSET="herdr-core-tui-${OS}-${ARCH}.tar.gz"

if [ -z "$TAG" ]; then
    EFFECTIVE_URL=$(curl -fsSIL -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")
    TAG="${EFFECTIVE_URL##*/}"
    [ -n "$TAG" ] || { echo "error: failed to resolve latest release tag" >&2; exit 1; }
fi

BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"
TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/herdr-core-tui-install.XXXXXX")
cleanup() {
    rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

echo "Downloading herdr-core-tui ${TAG} (${ASSET})..."
curl -fsSL "${BASE_URL}/${ASSET}" -o "${TMP_DIR}/${ASSET}"
curl -fsSL "${BASE_URL}/SHA256SUMS.txt" -o "${TMP_DIR}/SHA256SUMS.txt"

EXPECTED=$(awk -v file="$ASSET" '$2 == file || $2 == ("./" file) { print $1; exit }' "${TMP_DIR}/SHA256SUMS.txt")
[ -n "$EXPECTED" ] || { echo "error: checksum for ${ASSET} not found in SHA256SUMS.txt" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "${TMP_DIR}/${ASSET}" | awk '{ print $1 }')
elif command -v shasum >/dev/null 2>&1; then
    ACTUAL=$(shasum -a 256 "${TMP_DIR}/${ASSET}" | awk '{ print $1 }')
else
    echo "error: sha256sum or shasum is required to verify download" >&2
    exit 1
fi

if [ "$ACTUAL" != "$EXPECTED" ]; then
    echo "error: SHA-256 mismatch (expected ${EXPECTED}, got ${ACTUAL})" >&2
    exit 1
fi

mkdir -p "$BIN_DIR"
tar -xzf "${TMP_DIR}/${ASSET}" -C "${TMP_DIR}" herdr-core-tui
chmod 0755 "${TMP_DIR}/herdr-core-tui"
mv -f "${TMP_DIR}/herdr-core-tui" "${BIN_DIR}/herdr-core-tui"
ln -sf herdr-core-tui "${BIN_DIR}/hc-tui"

echo "Installed herdr-core-tui and hc-tui ${TAG} into ${BIN_DIR} (existing \`hc\` untouched)."
"${BIN_DIR}/herdr-core-tui" --version || true
