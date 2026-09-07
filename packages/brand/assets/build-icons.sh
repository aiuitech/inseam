#!/usr/bin/env bash
# Render the SVG brand sources in this directory into the binary assets the
# apps consume: icon-macos.icns (macOS app bundle), favicon.ico (web), and
# reference PNGs under png/. Requires ImageMagick (`magick`) and, for the
# .icns, macOS `iconutil`. Re-run after editing any source SVG; outputs are
# committed so app builds don't need these tools installed.
set -euo pipefail

cd "$(dirname "$0")"
command -v magick >/dev/null || { echo "error: ImageMagick 'magick' not found (brew install imagemagick)" >&2; exit 1; }

render() { # render <svg> <size WxH> <out.png>
    magick -background none -density 384 "$1" -resize "$2" "$3"
}

echo "==> png/"
mkdir -p png
render mark.svg        512x    png/mark-512.png
render stitch.svg      1024x   png/stitch-1024.png
render favicon.svg     512x512 png/favicon-512.png
render icon-macos.svg 1024x1024 png/icon-macos-1024.png

echo "==> favicon.ico"
magick \
    \( -background none -density 384 favicon.svg -resize 16x16 \) \
    \( -background none -density 384 favicon.svg -resize 32x32 \) \
    \( -background none -density 384 favicon.svg -resize 48x48 \) \
    favicon.ico

if command -v iconutil >/dev/null; then
    echo "==> icon-macos.icns"
    iconset=icon-macos.iconset
    rm -rf "$iconset" && mkdir "$iconset"
    for size in 16 32 128 256 512; do
        render icon-macos.svg "${size}x${size}" "$iconset/icon_${size}x${size}.png"
        double=$((size * 2))
        render icon-macos.svg "${double}x${double}" "$iconset/icon_${size}x${size}@2x.png"
    done
    iconutil -c icns "$iconset" -o icon-macos.icns
    rm -rf "$iconset"
else
    echo "==> skipping icon-macos.icns (iconutil not available)"
fi

echo "==> done"
