#!/usr/bin/env bash
# Package the release binaries for a target triple into a single bundle:
#   dist/factor-q-<version>-<target>.tar.gz   (+ .sha256)
# The archive holds every requested binary plus LICENSE/README when present.
#
# Usage: scripts/package.sh <target-triple> <spec> [<spec> ...]
# where each <spec> is either
#   <crate-dir>:<bin>   a built binary at <crate-dir>/target/<triple>/release/<bin>
#   <repo-rel-path>     a plain repo file copied into the bundle as-is
#                       (e.g. the dogfood launchers, so they travel with
#                       the binaries they launch — #102)
#   e.g. scripts/package.sh x86_64-unknown-linux-musl services/fq-runtime:fq services/fq-store:fq-cas
# (normally invoked via `just package <target>` / `just package-main <target>`).
set -euo pipefail

target="${1:?usage: package.sh <target> <crate-dir>:<bin> ...}"
shift
[ "$#" -ge 1 ] || {
    echo "usage: package.sh <target> <crate-dir>:<bin> ..." >&2
    exit 1
}

root="$(cd "$(dirname "$0")/.." && pwd)"
# The release version is the runtime workspace version, which
# `just check-version` ties to the release tag.
version="$(grep -m1 '^version = ' "$root/services/fq-runtime/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/')"

name="factor-q-${version}-${target}"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT

for spec in "$@"; do
    if [[ "$spec" != *:* ]]; then
        if [ ! -f "$root/$spec" ]; then
            echo "bundle file not found: $root/$spec" >&2
            exit 1
        fi
        cp "$root/$spec" "$stage/"
        continue
    fi
    crate_dir="${spec%%:*}"
    bin="${spec##*:}"
    bin_path="$root/$crate_dir/target/$target/release/$bin"
    if [ ! -x "$bin_path" ]; then
        echo "binary not found: $bin_path (run 'just build-release $target' first)" >&2
        exit 1
    fi
    cp "$bin_path" "$stage/$bin"
done

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

echo "packaged dist/${name}.tar.gz ($*)"
