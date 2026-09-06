// swift-tools-version: 5.10
import PackageDescription

// The model layer (FFI wrapper, config mirrors, secrets, brand) lives in the
// shared InseamKit package next door; this package keeps only the macOS
// views and AppModel. The Rust core links in through InseamKit's CInseamFFI
// target; the library search path for libinseam_ffi.a is supplied by
// build.sh (-Xlinker -L<repo>/target/release), keeping this manifest path-free.
let package = Package(
    name: "Inseam",
    platforms: [.macOS(.v14)],
    dependencies: [
        .package(path: "../inseamkit"),
    ],
    targets: [
        .executableTarget(
            name: "Inseam",
            dependencies: [
                .product(name: "InseamKit", package: "inseamkit"),
            ],
            path: "Sources/Inseam"
        ),
    ]
)
