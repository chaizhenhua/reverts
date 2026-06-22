#!/usr/bin/env bash
# Build release `reverts-cli` binaries and package them into the exact tarball
# layout the `release` CI job publishes and `install.sh` consumes:
#
#   dist/reverts-<target>.tar.gz          # contains reverts-<target>/bin/reverts-cli
#   dist/reverts-<target>.tar.gz.sha256   #          + reverts-<target>/skills/...
#                                         #          + reverts-<target>/README.md
#
# This is the local mirror of .github/workflows/release.yml — use it to test a
# release end-to-end before tagging, or to produce artifacts off-CI.
#
# Usage:
#   scripts/cross-build.sh                 # host target only (native build)
#   scripts/cross-build.sh --all           # every supported target
#   scripts/cross-build.sh <triple> ...    # explicit target list
#
# Targets that are not host-native need a cross linker. If `cross` is installed
# (https://github.com/cross-rs/cross) it is used automatically for those; plain
# `cargo` is used for the host target. Non-host targets without `cross` are
# skipped with a warning rather than failing the whole run.

set -euo pipefail

cd "$(dirname "$0")/.."
repo_root="$(pwd)"

SUPPORTED="x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin"
host="$(rustc -vV | awk '/^host:/ {print $2}')"

# Resolve the requested target list.
targets=""
case "${1:-}" in
    --all) targets="$SUPPORTED" ;;
    "")    targets="$host" ;;
    *)     targets="$*" ;;
esac

dist="${repo_root}/dist"
mkdir -p "$dist"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" > "$1.sha256"
    else shasum -a 256 "$1" > "$1.sha256"; fi
}

built=""
skipped=""
for target in $targets; do
    echo "==> $target"
    rustup target add "$target" >/dev/null 2>&1 || true

    if [ "$target" = "$host" ]; then
        cargo build --release --locked --bin reverts-cli --target "$target"
    elif command -v cross >/dev/null 2>&1; then
        cross build --release --locked --bin reverts-cli --target "$target"
    else
        echo "    skip: $target is not host-native and \`cross\` is not installed" >&2
        skipped="$skipped $target"
        continue
    fi

    bin="target/${target}/release/reverts-cli"
    [ -f "$bin" ] || { echo "    error: missing $bin" >&2; exit 1; }

    # Package identically to the CI `release` job.
    stage="reverts-${target}"
    stage_dir="${dist}/${stage}"
    rm -rf "$stage_dir"
    mkdir -p "${stage_dir}/bin"
    cp "$bin" "${stage_dir}/bin/reverts-cli"
    cp -R skills "${stage_dir}/skills"
    cp README.md "${stage_dir}/README.md"

    tarball="${dist}/${stage}.tar.gz"
    tar -czf "$tarball" -C "$dist" "$stage"
    rm -rf "$stage_dir"
    sha256 "$tarball"
    echo "    packaged $(basename "$tarball")"
    built="$built $target"
done

echo
echo "built:  ${built:- (none)}"
[ -n "$skipped" ] && echo "skipped:${skipped}"
echo "artifacts in: $dist"
