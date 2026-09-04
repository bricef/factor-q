#!/bin/sh
# factor-q installer. Detects your platform, downloads the matching release
# bundle from GitHub (a single archive with the fq, fqd and fq-cas binaries),
# verifies its checksum, and installs them.
#
#   curl -fsSL https://raw.githubusercontent.com/bricef/factor-q/main/install.sh | sh
#
# Verification fails closed: the bundle's published `.sha256` must be
# fetched and must match, and a SHA-256 tool must be present — none of
# the three is optional, and no failure of any of them installs anything
# (https://github.com/bricef/factor-q/issues/405).
#
# Environment overrides:
#   FQ_VERSION       version to install (e.g. 0.1.0 or v0.1.0; default: latest)
#   FQ_INSTALL_DIR   install directory (default: $HOME/.local/bin)
#   FQ_RELEASE_BASE  where the release assets are fetched from — a mirror,
#                    or a local server when testing this script (default:
#                    https://github.com/bricef/factor-q/releases/download)
set -eu

REPO="bricef/factor-q"
INSTALL_DIR="${FQ_INSTALL_DIR:-$HOME/.local/bin}"
RELEASE_BASE="${FQ_RELEASE_BASE:-https://github.com/$REPO/releases/download}"

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

# --- pick the checksum tool up front: no tool, no install ---
if command -v sha256sum >/dev/null 2>&1; then
    sha256_of() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256_of() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    err "required command not found: sha256sum or shasum (needed to verify the download; refusing to install unverified)"
fi

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
url="${RELEASE_BASE}/${tag}/${name}.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Installing factor-q ${tag} (${target}) -> ${INSTALL_DIR}"
curl -fsSL "$url" -o "$tmp/bundle.tar.gz" || err "download failed: $url"

# --- verify the checksum; every failure here aborts the install ---
# The .sha256 is mandatory. Installing when it cannot be fetched would
# hand anyone who can make one request fail (a MITM, a CDN blip, a
# mispublished release) an unverified install, silently.
curl -fsSL "${url}.sha256" -o "$tmp/bundle.sha256" \
    || err "could not fetch the checksum file ${url}.sha256 — refusing to install an unverified bundle (the release may be mispublished, or the connection interfered with; retry, or check https://github.com/$REPO/releases)"
expected="$(awk 'NF { print $1; exit }' "$tmp/bundle.sha256")"
case "$expected" in
    *[!0-9a-fA-F]* | "") err "checksum file ${url}.sha256 is malformed (expected a SHA-256 hex digest, got '$expected') — refusing to install" ;;
esac
[ "${#expected}" -eq 64 ] || err "checksum file ${url}.sha256 is malformed (expected 64 hex chars, got ${#expected}) — refusing to install"
actual="$(sha256_of "$tmp/bundle.tar.gz")"
[ "$expected" = "$actual" ] || err "checksum mismatch for $url (expected $expected, got $actual) — refusing to install"
echo "  checksum ok"

tar -xzf "$tmp/bundle.tar.gz" -C "$tmp"

mkdir -p "$INSTALL_DIR"
for bin in fq fqd fq-cas; do
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
echo "('fqd' is the daemon — 'fq' is a client and cannot start one.)"
echo "(fq-cas is the content-addressed storage CLI: 'fq-cas --help'.)"
