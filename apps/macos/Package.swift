// swift-tools-version: 5.10
import PackageDescription

// The Rust core links in via the CInseamFFI system-library target; the
// library search path for libinseam_ffi.a is supplied by build.sh
// (-Xlinker -L<repo>/target/release), keeping this manifest path-free.
let package = Package(
    name: "Inseam",
    platforms: [.macOS(.v14)],
    targets: [
        .systemLibrary(name: "CInseamFFI", path: "Sources/CInseamFFI"),
        .executableTarget(
            name: "Inseam",
            dependencies: ["CInseamFFI"],
            path: "Sources/Inseam"
        ),
    ]
)
