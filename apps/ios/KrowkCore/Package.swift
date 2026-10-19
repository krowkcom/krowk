// swift-tools-version: 6.0
// The iPhone app's Rust core (R-IOS-14, R-CLIENT-1): krowk-ffi's static
// XCFramework and the Swift bindings UniFFI generates for it, both under
// build/, which `make ios-core` writes and git ignores. Zero third-party
// packages. The generated Krowk.swift (UniFFI 0.32) compiles in Swift 6
// language mode as it is.
import PackageDescription

let package = Package(
    name: "KrowkCore",
    platforms: [.iOS("26.0")],
    products: [.library(name: "Krowk", targets: ["Krowk"])],
    targets: [
        .binaryTarget(name: "KrowkFFI", path: "build/KrowkFFI.xcframework"),
        .target(name: "Krowk", dependencies: ["KrowkFFI"], path: "build/Sources/Krowk"),
        .testTarget(name: "KrowkCoreTests", dependencies: ["Krowk"], path: "Tests/KrowkCoreTests"),
    ]
)
