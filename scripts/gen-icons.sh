#!/usr/bin/env bash
# Regenerates the raster icons in assets/brand from the SVG sources.
#   same-icon.svg : app icon with its background tile (desktop / taskbar / favicon)
#   shark-egg.svg : Shark Egg (mascot) alone on a transparent background (README, in-app)
# Needs ImageMagick (`convert`).
set -euo pipefail
cd "$(dirname "$0")/../assets/brand"

render() { # svg size out
  convert -background none -density 384 "$1" -filter Lanczos -resize "$2x$2" \
    -depth 8 PNG32:"$3"
}

mkdir -p png
for s in 16 24 32 48 64 128 256 512 1024; do
  render same-icon.svg "$s" "png/same-icon-$s.png"
done
for s in 128 256 512; do
  render shark-egg.svg "$s" "png/shark-egg-$s.png"
done

# Windows app icon (embedded in same.exe) and a web favicon.
convert png/same-icon-{16,24,32,48,64,128,256}.png same.ico
convert png/same-icon-{16,32,48}.png favicon.ico
echo "Generated icons in $(pwd)"
