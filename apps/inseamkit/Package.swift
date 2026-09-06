// swift-tools-version: 5.10
import PackageDescription

// InseamKit is the platform-neutral Swift face of the node: the FFI wrapper,
// the Codable mirrors of the core's messages, the Keychain secret store, the
// brand tokens, and the SwiftUI pieces every app shell shares. The macOS and
// iOS apps keep only their views and app models.
//
// The Rust core links in via the CInseamFFI system-library target; the
// library search path for libinseam_ffi.a is supplied by the caller
// (-Xlinker -L<repo>/target/release), keeping this manifest path-free.
let package = Package(
    name: "InseamKit",
    platforms: [.macOS(.v14), .iOS(.v17)],
    products: [
        .library(name: "InseamKit", targets: ["InseamKit"]),
    ],
    targets: [
        .systemLibrary(name: "CInseamFFI", path: "Sources/CInseamFFI"),
        .target(
            name: "InseamKit",
            dependencies: ["CInseamFFI"],
            path: "Sources/InseamKit"
        ),
        .testTarget(
            name: "InseamKitTests",
            dependencies: ["InseamKit"],
            path: "Tests/InseamKitTests"
        ),
    ]
)
