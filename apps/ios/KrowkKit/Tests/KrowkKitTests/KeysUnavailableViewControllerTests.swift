// R-IOS-8: with no Keychain storage the app says why instead of keeping
// keys anywhere weaker, and tells a phone with no Secure Enclave apart from
// a failure that may pass.
import Testing
import UIKit
@testable import KrowkKit

@MainActor
@Suite("R-IOS-8: the screen shown when the keys can't be opened")
struct KeysUnavailableViewControllerTests {
    @Test("R-IOS-8: no Secure Enclave says the iPhone can't keep krowk's keys safely, and why")
    func noSecureEnclave() {
        let screen = KeysUnavailableViewController(cause: .noSecureEnclave, reason: "no enclave here")
        screen.loadViewIfNeeded()
        #expect(screen.headline.text == "This iPhone can't keep krowk's keys safely.")
        #expect(screen.reasonLabel.text == "no enclave here")
        #expect(screen.reasonLabel.accessibilityIdentifier == "keys-unavailable")
        #expect(screen.title == "krowk")
    }

    @Test("R-IOS-8: any other failure says the keys couldn't be opened, not that the iPhone can't keep them")
    func otherFailure() {
        let screen = KeysUnavailableViewController(cause: .failed, reason: "OSStatus -25308 (the iPhone is locked)")
        screen.loadViewIfNeeded()
        #expect(screen.headline.text == "krowk couldn't open its keys.")
        #expect(screen.reasonLabel.text == "OSStatus -25308 (the iPhone is locked)")
    }
}
