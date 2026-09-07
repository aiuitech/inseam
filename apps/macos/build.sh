#!/usr/bin/env bash
# Build the Inseam macOS app: Rust core (staticlib) + SwiftUI shell, assembled
# into a .app bundle. By default the bundle lands in /Applications so the
# result is previewable directly; pass a different destination directory as $1.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP_SRC="$ROOT/apps/macos"
DEST="${1:-/Applications}"

echo "==> cargo build -p inseam-ffi --release"
cargo build -p inseam-ffi --release --manifest-path "$ROOT/Cargo.toml"

echo "==> swift build -c release"
swift build -c release --package-path "$APP_SRC" \
    -Xlinker -L"$ROOT/target/release"

BIN="$(swift build -c release --package-path "$APP_SRC" --show-bin-path)/Inseam"
APP="$DEST/Inseam.app"

echo "==> assembling $APP"
mkdir -p "$APP/Contents/MacOS"
cp "$APP_SRC/Info.plist" "$APP/Contents/Info.plist"
printf 'APPL????' > "$APP/Contents/PkgInfo"
cp "$BIN" "$APP/Contents/MacOS/Inseam"
mkdir -p "$APP/Contents/Resources"
cp "$ROOT/packages/brand/assets/icon-macos.icns" "$APP/Contents/Resources/Inseam.icns"
codesign --force --sign - "$APP"

echo "==> done: $APP"
