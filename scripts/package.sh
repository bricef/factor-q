#!/usr/bin/env bash
# Package a release binary for a target triple into dist/:
#   dist/<bin>-<version>-<target>.tar.gz   (+ .sha256)
# The archive holds the binary plus LICENSE/README files when present.
#
# Usage: scripts/package.sh <target-triple> <crate-dir> <bin-name>
#   e.g. scripts/package.sh x86_64-unknown-linux-musl services/fq-runtime fq
# (normally invoked via `just package <target>`).
set -euo pipefail

target="${1:?usage: package.sh <target> <crate-dir> <bin-name>}"
crate_dir="${2:?usage: package.sh <target> <crate-dir> <bin-name>}"
bin_name="${3:?usage: package.sh <target> <crate-dir> <bin-name>}"
root="$(cd "$(dirname "$0")/.." && pwd)"
version="$(grep -m1 '^version = ' "$root/$crate_dir/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"
bin="$root/$crate_dir/target/$target/release/$bin_name"

if [ ! -x "$bin" ]; then
    echo "binary not found: $bin (run 'just build-release $target' first)" >&2
    exit 1
fi

name="${bin_name}-${version}-${target}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

cp "$bin" "$stage/$bin_name"
# Bundle license + readme when they exist (the LICENSE file lands once the
# license is chosen; packaging tolerates its absence until then).
for f in LICENSE LICENSE-MIT LICENSE-APACHE README.md; do
    [ -f "$root/$f" ] && cp "$root/$f" "$stage/"
done

mkdir -p "$root/dist"
tar -czf "$root/dist/${name}.tar.gz" -C "$stage" .

# Checksum (sha256sum on Linux, shasum on macOS).
(
    cd "$root/dist"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "${name}.tar.gz" >"${name}.tar.gz.sha256"
    else
        shasum -a 256 "${name}.tar.gz" >"${name}.tar.gz.sha256"
    fi
)

echo "packaged dist/${name}.tar.gz"
