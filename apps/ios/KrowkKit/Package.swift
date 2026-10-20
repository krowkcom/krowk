// swift-tools-version: 6.0
// The iPhone app's views and models (R-IOS-13, R-CLIENT-2): UIKit, Swift 6
// with complete strict concurrency, iOS 26, zero third-party packages. It
// does not import the Rust bindings: the app target passes in what it reads
// from them, so these tests need no XCFramework.
import PackageDescription

let package = Package(
    name: "KrowkKit",
    platforms: [.iOS("26.0")],
    products: [.library(name: "KrowkKit", targets: ["KrowkKit"])],
    targets: [
        .target(name: "KrowkKit"),
        .testTarget(name: "KrowkKitTests", dependencies: ["KrowkKit"]),
    ]
)
