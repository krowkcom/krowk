// R-CLIENT-2, R-IOS-13: the phone's shell is native UIKit, and its first
// screen shows the protocol version it is handed.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-CLIENT-2, R-IOS-13: the UIKit home screen")
struct HomeViewControllerTests {
    @Test("R-CLIENT-2, R-IOS-13: loading the view with version 1 shows \"Protocol 1\"")
    func showsTheProtocolVersion() {
        let home = HomeViewController(protocolVersion: 1)
        home.loadViewIfNeeded()
        #expect(home.versionLabel.text == "Protocol 1")
        #expect(home.versionLabel.accessibilityIdentifier == "protocol-version")
        #expect(home.versionLabel.superview === home.view)
    }

    @Test("R-IOS-13: the title is \"krowk\", shown large")
    func titleIsKrowk() {
        let home = HomeViewController(protocolVersion: 1)
        #expect(home.title == "krowk")
        #expect(home.navigationItem.largeTitleDisplayMode == .always)
    }
}
