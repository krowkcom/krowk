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
import PackageDescription

let package = Package(
    name: "KrowkKit",
    platforms: [.iOS("26.0")],
    products: [
        .library(name: "KrowkKit", targets: ["KrowkKit"]),
        .library(name: "KrowkKeys", targets: ["KrowkKeys"]),
    ],
    dependencies: [.package(path: "../KrowkCore")],
    targets: [
        .target(name: "KrowkKit"),
        .testTarget(name: "KrowkKitTests", dependencies: ["KrowkKit"]),
        .target(name: "KrowkKeys", dependencies: [.product(name: "Krowk", package: "KrowkCore")]),
    ]
)
