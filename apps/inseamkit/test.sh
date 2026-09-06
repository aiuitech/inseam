#!/usr/bin/env bash
# Run the InseamKit tests. The library links the Rust core, so the tests need
# libinseam_ffi.a (built by `cargo build -p inseam-ffi --release`) on the
# linker path. With only the Xcode Command Line Tools installed — the setup
# the app build requires — SwiftPM does not find Testing.framework on its
# own, so this script points the compiler, linker, and loader at the CLT copy.
# Under a full Xcode those paths do not exist and plain `swift test` is enough.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PACKAGE="$ROOT/apps/inseamkit"
DEVELOPER="$(xcode-select --print-path)"
FRAMEWORKS="$DEVELOPER/Library/Developer/Frameworks"
INTEROP_LIBRARY="$DEVELOPER/Library/Developer/usr/lib"
PLUGINS="$DEVELOPER/usr/lib/swift/host/plugins/testing"

if [[ ! -f "$ROOT/target/release/libinseam_ffi.a" ]]; then
    echo "==> cargo build -p inseam-ffi --release"
    cargo build -p inseam-ffi --release --manifest-path "$ROOT/Cargo.toml"
fi

TESTING_FLAGS=()
if [[ -d "$FRAMEWORKS/Testing.framework" ]]; then
    TESTING_FLAGS=(
        --enable-swift-testing
        -Xswiftc -F"$FRAMEWORKS"
        -Xswiftc -plugin-path -Xswiftc "$PLUGINS"
        -Xlinker -F"$FRAMEWORKS"
        -Xlinker -rpath -Xlinker "$FRAMEWORKS"
        -Xlinker -rpath -Xlinker "$INTEROP_LIBRARY"
    )
fi

echo "==> swift test"
swift test --package-path "$PACKAGE" \
    "${TESTING_FLAGS[@]}" \
    -Xlinker -L"$ROOT/target/release" \
    "$@"
