// swift-tools-version: 6.0
// The iPhone app's own code: Swift 6 with complete strict concurrency,
// iOS 26, zero third-party packages.
//
// KrowkKit is the views and models (R-IOS-13, R-CLIENT-2), UIKit. It does
// not import the Rust bindings: the app target passes in what it reads from
// them, so its tests need no XCFramework.
//
// KrowkKeys is where the phone keeps krowk's keys (R-IOS-8): the Keychain,
// behind the Secure Enclave, as the bindings' KeyStorage. It imports them,
// so resolving this package needs `make ios-core`'s build/ in KrowkCore.
// Its tests are not here but in ../AppTests, hosted in the app: the
// Keychain refuses an unhosted test bundle (errSecMissingEntitlement), as
// it has no application identifier.
//
// KrowkCache is the phone's on-device cache of the session index and the
// recent transcripts (R-IOS-1, R-IOS-3): the system's SQLite, plain Swift
// values and the events' JSON text, no bindings, so its tests are here.
//
// KrowkAccount is KrowkKit's sign-in, pairing, session list and live
// relay connection (Network.framework) over the bindings
// (R-IOS-9, R-E2E-3): the registry's browser sign-in, its key kept through
// a KeyStorage, joining the person's devices, and which screen a launch
// opens. Its tests are in ../AppTests too, for the same
// reason.
import PackageDescription

let package = Package(
    name: "KrowkKit",
    platforms: [.iOS("26.0")],
    products: [
        .library(name: "KrowkKit", targets: ["KrowkKit"]),
        .library(name: "KrowkKeys", targets: ["KrowkKeys"]),
        .library(name: "KrowkAccount", targets: ["KrowkAccount"]),
        .library(name: "KrowkCache", targets: ["KrowkCache"]),
    ],
    dependencies: [.package(path: "../KrowkCore")],
    targets: [
        .target(name: "KrowkKit"),
        .testTarget(name: "KrowkKitTests", dependencies: ["KrowkKit", "KrowkCache"]),
        .target(name: "KrowkCache", dependencies: ["KrowkKit"]),
        .target(name: "KrowkKeys", dependencies: [.product(name: "Krowk", package: "KrowkCore")]),
        .target(name: "KrowkAccount", dependencies: ["KrowkKit", "KrowkCache", .product(name: "Krowk", package: "KrowkCore")]),
    ]
)
