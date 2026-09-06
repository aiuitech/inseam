#!/usr/bin/env bash
# Build the Rust core for iOS: one static library per platform under
# Core/<platform>/, where the Xcode project's LIBRARY_SEARCH_PATHS looks.
#
# The iOS build turns the loaded-plugin tier off (`--no-default-features`):
# wasmtime's Cranelift JIT needs executable memory, which iOS denies to
# third-party apps (design/ios-app.md). Linked plugins and app-bridged hosts
# and providers are what an iOS node runs.
#
# Requires: Xcode (the iOS SDKs — the Command Line Tools alone are not
# enough), and the Rust targets:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP="$ROOT/apps/ios"
DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-26.0}"

if ! xcrun --sdk iphoneos --show-sdk-path >/dev/null 2>&1; then
    echo "error: the iOS SDK is not installed; install Xcode and run" >&2
    echo "  sudo xcode-select --switch /Applications/Xcode.app" >&2
    exit 1
fi

build_target() {
    local target="$1"
    local platform="$2"
    echo "==> cargo build -p inseam-ffi --release --target $target (no loaded plugins)"
    IPHONEOS_DEPLOYMENT_TARGET="$DEPLOYMENT_TARGET" \
    cargo build -p inseam-ffi --release --no-default-features \
        --target "$target" --manifest-path "$ROOT/Cargo.toml"
    mkdir -p "$APP/Core/$platform"
    cp "$ROOT/target/$target/release/libinseam_ffi.a" "$APP/Core/$platform/libinseam_ffi.a"
    echo "    → Core/$platform/libinseam_ffi.a"
}

build_target aarch64-apple-ios iphoneos
build_target aarch64-apple-ios-sim iphonesimulator

echo "==> done. Next: (cd apps/ios && xcodegen generate && open Inseam.xcodeproj)"
