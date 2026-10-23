// R-E2E-3, R-IOS-8, R-IOS-12 (ticket 117): an iPhone removed from the
// person's devices, end to end on the simulator against the removal tests'
// own fake fleet (removing a device rotates host A's user key, which would
// stop the shared fleet's hosts). The phone signs in and pairs; the paired
// screen reminds of the recovery kit in the terminal's words (the fleet's
// list has none), once: a relaunch opens Sessions with no reminder. Host A
// runs `krowk devices remove <the phone>`; the app says "This iPhone was
// removed from your devices" in place of Sessions, and the registry holds
// no push token for it. "Reset this iPhone" opens sign-in, and so does the
// next launch: its keys are gone. Screenshots of the reminder and the
// removed screen, light and dark, are kept in the results.
//
// Needs the fleets: apps/ios/scripts/with-fleet.sh hands the removal
// fleet's URLs to this runner as KROWK_REMOVAL_API_URL,
// KROWK_REMOVAL_RELAY_URL, KROWK_REMOVAL_PAIR_URL and
// KROWK_REMOVAL_DEVICES_URL.
import XCTest

final class RemovalUITests: XCTestCase {
    override func setUp() {
        continueAfterFailure = false
    }

    @MainActor
    func testR_E2E_3_RemovedPhoneSaysSoAndResetsToSignIn() async throws {
        addTeardownBlock { @MainActor in
            FleetUI.forgetTheKeys()
            XCUIDevice.shared.appearance = .light
        }
        for appearance in Screenshots.appearances {
            XCUIDevice.shared.appearance = appearance
            try await removed(appearance)
        }
    }

    @MainActor
    private func removed(_ appearance: XCUIDevice.Appearance) async throws {
        let api = try FleetUI.env("KROWK_REMOVAL_API_URL")
        let site = api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
        let name = "Removed iPhone \(UUID().uuidString.prefix(8))"
        let app = XCUIApplication()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = "1"
        app.launchEnvironment["KROWK_API_URL"] = api
        app.launchEnvironment["KROWK_RELAY_URL"] = try FleetUI.env("KROWK_REMOVAL_RELAY_URL")
        app.launchEnvironment["KROWK_APPEARANCE"] = Screenshots.name(of: appearance)
        app.launchEnvironment["KROWK_DEVICE_NAME"] = name
        // GitHub's simulators get no APNs token: the DEBUG build hands this
        // one back for iOS.
        app.launchEnvironment["KROWK_FAKE_APNS_TOKEN"] = (0..<32).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
        app.launch()

        // Sign in and pair, as PairingUITests does.
        let signIn = app.buttons["sign-in"]
        XCTAssertTrue(signIn.waitForExistence(timeout: 10), "no Sign in button")
        signIn.tap()
        let code = try XCTUnwrap(FleetUI.signInCode(in: app), "no sign-in code shown")
        XCTAssertTrue(FleetUI.allowTheSignInPage(until: app.staticTexts[code]), "the page in the sheet does not show \(code)")
        let (approved, why) = try await FleetUI.request("\(site)/_approve/cli/authorizations/\(code)/approval")
        XCTAssertEqual(approved, 200, why)
        let field = app.textFields["pairing-code"]
        if !field.waitForExistence(timeout: 30) { Screenshots.tree(of: app) }
        XCTAssertTrue(field.exists, "no pairing screen 30 s after approving")
        let tokensBefore = try await NotificationPrompt.registered(site: site)
        let (status, fresh) = try await FleetUI.request(try FleetUI.env("KROWK_REMOVAL_PAIR_URL"))
        XCTAssertEqual(status, 200, fresh)
        field.tap()
        field.typeText(String(fresh.dropLast()))
        app.typeText(String(fresh.suffix(1)))
        let done = app.staticTexts["pairing-done"]
        if !done.waitForExistence(timeout: 90) { Screenshots.tree(of: app) }
        XCTAssertTrue(done.exists, "not paired 90 s after the code")

        // The recovery kit, in the terminal's words: this list has none.
        let kit = app.staticTexts["pairing-kit"]
        XCTAssertTrue(kit.waitForExistence(timeout: 10), "no recovery kit reminder once paired")
        XCTAssertTrue(kit.label.hasPrefix("No recovery kit: if you lose every device you lose every session"), kit.label)
        NotificationPrompt.allow(timeout: 5)
        Screenshots.keep("paired-recovery-kit", in: app)

        // The token reaches the registry, for this phone.
        var device: String?
        let registeredBy = Date.now.addingTimeInterval(60)
        while device == nil && Date.now < registeredBy {
            device = try await NotificationPrompt.registered(site: site).keys.first { tokensBefore[$0] == nil }
            if device == nil { try await Task.sleep(for: .milliseconds(500)) }
        }
        let phone = try XCTUnwrap(device, "no push token registered for the new phone within 60 s")

        app.buttons["pairing-continue"].tap()
        let list = app.collectionViews["sessions-list"]
        XCTAssertTrue(list.waitForExistence(timeout: 10), "Continue did not open Sessions")
        XCTAssertFalse(app.staticTexts["pairing-kit"].exists)

        // Once: a relaunch opens Sessions, with no reminder.
        app.terminate()
        app.launchEnvironment["KROWK_FIRST_LAUNCH"] = nil
        app.launch()
        XCTAssertTrue(list.waitForExistence(timeout: 20), "a relaunch did not open Sessions")
        XCTAssertFalse(app.staticTexts["pairing-kit"].exists, "the reminder showed again")

        // Host A removes the phone. Each pull reads the list, and every
        // read checks the device list first.
        let (removedStatus, removedReply) = try await FleetUI.request(try FleetUI.env("KROWK_REMOVAL_DEVICES_URL") + "/remove", body: name)
        XCTAssertEqual(removedStatus, 200, removedReply)
        let title = app.staticTexts["removed-title"]
        let deadline = Date.now.addingTimeInterval(90)
        while !title.waitForExistence(timeout: 3) && Date.now < deadline {
            if list.exists { list.swipeDown() }
        }
        if !title.exists { Screenshots.tree(of: app) }
        XCTAssertTrue(title.exists, "the app never said it was removed")
        XCTAssertEqual(title.label, "This iPhone was removed from your devices")
        XCTAssertFalse(list.exists, "Sessions is still shown")
        XCTAssertTrue(app.buttons["removed-reset"].exists)
        Screenshots.keep("removed", in: app)
        // R-IOS-12: no token for it at the registry.
        let after = try await NotificationPrompt.registered(site: site)
        XCTAssertNil(after[phone], "the removed phone still has a push token")

        // Reset: sign-in, now and at the next launch.
        app.buttons["removed-reset"].tap()
        XCTAssertTrue(signIn.waitForExistence(timeout: 30), "the reset did not open sign-in")
        app.terminate()
        app.launch()
        XCTAssertTrue(signIn.waitForExistence(timeout: 20), "a launch after the reset did not open sign-in: its keys are kept")
        XCTAssertFalse(list.exists)
        app.terminate()
    }
}
