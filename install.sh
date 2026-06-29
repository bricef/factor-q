#!/bin/sh
# factor-q installer. Detects your platform, downloads the matching release
# bundle from GitHub (a single archive with the fq and fq-cas binaries),
# verifies its checksum, and installs them.
#
#   curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/install.sh | sh
#
# Environment overrides:
#   FQ_VERSION       version to install (e.g. 0.1.0 or v0.1.0; default: latest)
#   FQ_INSTALL_DIR   install directory (default: $HOME/.local/bin)
set -eu

REPO="bricef/factor-q"
INSTALL_DIR="${FQ_INSTALL_DIR:-$HOME/.local/bin}"

err() {
    echo "error: $*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || err "required command not found: $1"
}

need curl
need tar
need uname

# --- detect target triple ---
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux)
        case "$arch" in
            x86_64 | amd64) target="x86_64-unknown-linux-musl" ;;
            aarch64 | arm64) target="aarch64-unknown-linux-musl" ;;
            *) err "unsupported Linux architecture: $arch" ;;
        esac
        ;;
    Darwin)
        case "$arch" in
            arm64) target="aarch64-apple-darwin" ;;
            x86_64) err "Intel macOS has no pre-built binary; build from source with 'cargo install --git https://github.com/$REPO fq-cli', or use an Apple Silicon Mac" ;;
            *) err "unsupported macOS architecture: $arch" ;;
        esac
        ;;
    *) err "unsupported OS: $os (Linux and macOS only)" ;;
esac

# --- resolve version ---
if [ -n "${FQ_VERSION:-}" ]; then
    tag="$FQ_VERSION"
    case "$tag" in v*) ;; *) tag="v$tag" ;; esac
else
    tag="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" \
        | grep '"tag_name"' | head -1 \
        | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/')"
    [ -n "$tag" ] || err "could not determine the latest release; set FQ_VERSION"
fi
version="${tag#v}"

name="factor-q-${version}-${target}"
url="https://github.com/$REPO/releases/download/${tag}/${name}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Installing factor-q ${tag} (${target}) -> ${INSTALL_DIR}"
curl -fsSL "$url" -o "$tmp/bundle.tar.gz" || err "download failed: $url"

# --- verify checksum when the .sha256 is published ---
if curl -fsSL "${url}.sha256" -o "$tmp/bundle.sha256" 2>/dev/null; then
    expected="$(awk '{print $1}' "$tmp/bundle.sha256")"
    if command -v sha256sum >/dev/null 2>&1; then
        actual="$(sha256sum "$tmp/bundle.tar.gz" | awk '{print $1}')"
    else
        actual="$(shasum -a 256 "$tmp/bundle.tar.gz" | awk '{print $1}')"
    fi
    [ "$expected" = "$actual" ] || err "checksum mismatch (expected $expected, got $actual)"
    echo "  checksum ok"
fi

tar -xzf "$tmp/bundle.tar.gz" -C "$tmp"

mkdir -p "$INSTALL_DIR"
for bin in fq fq-cas; do
    [ -f "$tmp/${bin}" ] || err "archive did not contain the ${bin} binary"
    if ! install -m 0755 "$tmp/${bin}" "$INSTALL_DIR/${bin}" 2>/dev/null; then
        cp "$tmp/${bin}" "$INSTALL_DIR/${bin}"
        chmod 0755 "$INSTALL_DIR/${bin}"
    fi
    echo "  installed $INSTALL_DIR/${bin}"
done

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "Note: $INSTALL_DIR is not on your PATH. Add it, e.g.:"
        echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac
echo "Run 'fq version' to verify, then 'fq init' to start a project."
echo "(fq-cas is the content-addressed storage CLI: 'fq-cas --help'.)"
