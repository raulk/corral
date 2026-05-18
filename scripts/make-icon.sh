#!/usr/bin/env bash
# Build the Corral app icon. Expands the source SVG to all macOS iconset sizes,
# then packs them into `crates/corral-app/resources/AppIcon.icns` via iconutil.
#
# Idempotent: re-running overwrites the generated icon.

set -euo pipefail

repo_root="$(cd "$(dirname "$0")/.." && pwd)"
svg="$repo_root/crates/corral-app/resources/icon.svg"
out="$repo_root/crates/corral-app/resources/AppIcon.icns"
work="$(mktemp -d -t corral-icon)"
trap 'rm -rf "$work"' EXIT

iconset="$work/AppIcon.iconset"
mkdir -p "$iconset"

if ! command -v rsvg-convert >/dev/null 2>&1; then
    echo "rsvg-convert not found; install via 'brew install librsvg'" >&2
    exit 1
fi

# (logical size, physical pixels, suffix) per Apple's iconset spec.
declare -a sizes=(
    "16    16    icon_16x16.png"
    "32    32    icon_16x16@2x.png"
    "32    32    icon_32x32.png"
    "64    64    icon_32x32@2x.png"
    "128   128   icon_128x128.png"
    "256   256   icon_128x128@2x.png"
    "256   256   icon_256x256.png"
    "512   512   icon_256x256@2x.png"
    "512   512   icon_512x512.png"
    "1024  1024  icon_512x512@2x.png"
)

for spec in "${sizes[@]}"; do
    read -r _logical pixels name <<<"$spec"
    rsvg-convert -w "$pixels" -h "$pixels" "$svg" -o "$iconset/$name"
done

iconutil -c icns -o "$out" "$iconset"
echo "wrote $out"
