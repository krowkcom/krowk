// swift-tools-version: 6.0
// The iPhone app for xtool (R-IOS-13), so Linux builds it with the
// omarchy-apple-dev toolchain: `xtool dev build` here reads this and
// xtool.yml. It compiles the very sources project.yml gives Xcode (App/,
// KrowkKit, KrowkKeys, KrowkAccount and KrowkCache, KrowkCore); keep the two in step,
// and keep this one thin. Swift 6 language mode, so strict concurrency is
// complete. Zero third-party packages.
import PackageDescription

let package = Package(
    name: "KrowkApp",
    platforms: [.iOS("26.0")],
    // xtool packs the one library product as the app; its name is the
    // executable's and the module's, KrowkApp as in project.yml.
    products: [.library(name: "KrowkApp", targets: ["KrowkApp"])],
    dependencies: [
        .package(path: "KrowkCore"),
        .package(path: "KrowkKit"),
    ],
    targets: [
        .target(
            name: "KrowkApp",
            dependencies: [
                .product(name: "Krowk", package: "KrowkCore"),
                .product(name: "KrowkKit", package: "KrowkKit"),
                .product(name: "KrowkKeys", package: "KrowkKit"),
                .product(name: "KrowkAccount", package: "KrowkKit"),
                .product(name: "KrowkCache", package: "KrowkKit"),
            ],
            path: "App",
            // ship.sh compiles the app icon from here with actool (README.md → Releasing to TestFlight).
            exclude: ["Assets.xcassets"],
            // xtool's darwin toolset links with -all_load (for dynamic
            // products), which pulls every member of libkrowk_ffi.a in,
            // Rust's compiler_builtins too, and those clash with
            // libclang_rt's. The app has no dynamic products; the linker
            // takes the last of the two flags. Xcode never sees this file.
            linkerSettings: [.unsafeFlags(["-Xlinker", "-noall_load"])]
        ),
    ]
)
