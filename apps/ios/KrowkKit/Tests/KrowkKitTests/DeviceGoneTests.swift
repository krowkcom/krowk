// R-E2E-3 (ticket 117): a phone no longer on the person's devices, against
// stubs: the watch that finds it and then stops, the session list that
// stops reading, the reset's order, and the screen's one action. The same
// against the fake fleet, a real removal by host A, is AppTests'
// RemovalTests and UITests' RemovalUITests.
import Foundation
import Synchronization
import Testing
@testable import KrowkKit

@Suite("R-E2E-3: a removed iPhone stops and offers a reset, stubbed")
@MainActor
struct DeviceGoneTests {
    nonisolated static let removed = DeviceGone.removed(detail: "this iPhone is no longer on your device list")

    /// A check that answers nil `onList` times, then gone, counting calls.
    final class Check: Sendable {
        private let state: Mutex<(calls: Int, onList: Int)>
        init(onList: Int) { state = Mutex((0, onList)) }
        var calls: Int { state.withLock { $0.calls } }
        func run() -> DeviceGone? {
            state.withLock { s in
                s.calls += 1
                return s.calls > s.onList ? DeviceGoneTests.removed : nil
            }
        }
    }

    @Test("R-E2E-3: the watch checks at once and each period; gone, it tells once and never checks again")
    func watchStopsOnceGone() async throws {
        let check = Check(onList: 2)
        let told = Box<[DeviceGone]>([])
        let watch = StandingWatch(check: { check.run() }, period: .milliseconds(20)) { told.value.append($0) }
        watch.start()
        try await until { told.value.count == 1 }
        #expect(told.value == [Self.removed])
        #expect(watch.gone == Self.removed)
        let calls = check.calls
        #expect(calls == 3)
        // Nothing more: not the timer, not a return to the foreground.
        watch.stop()
        watch.start()
        try await Task.sleep(for: .milliseconds(150))
        #expect(check.calls == calls, "checked again after it was found gone")
        #expect(told.value.count == 1)
    }

    @Test("R-E2E-3: gone, as another part of the app found it, stops the watch and is told once")
    func reportedOnce() async throws {
        let check = Check(onList: .max)
        let told = Box(0)
        let watch = StandingWatch(check: { check.run() }, period: .milliseconds(20)) { _ in told.value += 1 }
        watch.start()
        try await until { check.calls >= 1 }
        watch.report(Self.removed)
        watch.report(.listReset(detail: "started over"))
        #expect(told.value == 1)
        #expect(watch.gone == Self.removed)
        let calls = check.calls
        try await Task.sleep(for: .milliseconds(150))
        #expect(check.calls <= calls + 1, "a check under way may land; none starts after")
    }

    @Test("R-E2E-3: the session list stops reading once its read finds the phone gone, and says so")
    func listStops() async throws {
        let source = StubSource(.failure(.gone(Self.removed)))
        let logged = Box<[String]>([])
        let model = SessionListModel(source: source, log: { logged.value.append($0) })
        model.appeared()
        try await until { model.gone != nil }
        #expect(model.gone == Self.removed)
        #expect(model.failure == .gone(Self.removed))
        #expect(model.failure?.sentence == "This iPhone was removed from your devices.")
        #expect(logged.value == ["listing sessions failed: device_removed"], "the kind, never the core's words")
        let reads = source.calls.count
        await model.refresh()
        model.appeared()
        await model.settled()
        try await Task.sleep(for: .milliseconds(100))
        #expect(source.calls.count == reads, "read again after the phone was found gone")
    }

    @Test("R-E2E-3: the reset clears the push token first, then what push kept, then every key")
    func resetOrder() async throws {
        let steps = Box<[String]>([])
        let reset = PhoneReset(
            clearPushToken: { steps.value.append("clear the registry's token") },
            forgetPush: { steps.value.append("forget push") },
            forgetKeys: { steps.value.append("forget the keys") }
        )
        try await reset.run()
        #expect(steps.value == ["clear the registry's token", "forget push", "forget the keys"])
    }

    @Test("R-E2E-3: a push token the registry won't clear (it refuses a removed phone's key) doesn't stop the reset, and is logged")
    func resetGoesOnWithoutTheRegistry() async throws {
        struct Refused: Error {}
        let steps = Box<[String]>([])
        let logged = Box<[String]>([])
        let reset = PhoneReset(
            clearPushToken: { throw Refused() },
            forgetPush: { steps.value.append("forget push") },
            forgetKeys: { steps.value.append("forget the keys") },
            log: { logged.value.append($0) }
        )
        try await reset.run()
        #expect(steps.value == ["forget push", "forget the keys"])
        #expect(logged.value.count == 1)
    }

    @Test("R-E2E-3: the screen resets only when tapped; a Keychain that refuses says so and may be tried again")
    func screenResets() async throws {
        struct Locked: Error, CustomStringConvertible { var description: String { "the iPhone is locked" } }
        let fail = Box(true)
        let forgotten = Box(0)
        let reset = PhoneReset(
            clearPushToken: {}, forgetPush: {},
            forgetKeys: {
                if fail.value { throw Locked() }
                forgotten.value += 1
            }
        )
        let done = Box(0)
        let model = DeviceGoneModel(gone: Self.removed, reset: reset, onReset: { done.value += 1 })
        #expect(model.gone.title == "This iPhone was removed from your devices")
        #expect(forgotten.value == 0 && done.value == 0, "nothing is reset before the tap")
        model.resetPhone()
        try await until { model.failure != nil }
        #expect(model.failure?.contains("the iPhone is locked") == true)
        #expect(done.value == 0 && !model.resetting)
        fail.value = false
        model.resetPhone()
        try await until { done.value == 1 }
        #expect(forgotten.value == 1)
        #expect(model.failure == nil)
    }

    @Test("R-E2E-3: Check again goes back to Sessions when the phone is listed after all, and resets nothing either way")
    func checkAgain() async throws {
        let forgotten = Box(0)
        let reset = PhoneReset(clearPushToken: {}, forgetPush: {}, forgetKeys: { forgotten.value += 1 })
        let answer = Box<Standing>(.gone(Self.removed))
        let listed = Box(0)
        let model = DeviceGoneModel(
            gone: Self.removed, reset: reset, recheck: { answer.value }, onReset: { Issue.record("reset without a tap") },
            onListed: { listed.value += 1 })
        model.checkAgain()
        try await until { !model.checking && model.failure != nil }
        #expect(model.failure == "Checked again: this iPhone is still not on your devices.")
        answer.value = .unknown("krowk's registry cannot be reached")
        model.checkAgain()
        try await until { !model.checking && model.failure?.hasPrefix("Couldn't check again") == true }
        #expect(listed.value == 0)
        answer.value = .listed
        model.checkAgain()
        try await until { listed.value == 1 }
        #expect(forgotten.value == 0)
    }

    @Test("R-E2E-3: a list started over says so in its own words")
    func listResetWords() {
        let gone = DeviceGone.listReset(detail: "your device list was started over on 2027-01-15 at 08:00 UTC")
        #expect(gone.title == "Your device list was started over")
        #expect(gone.kind == "device_list_reset")
        #expect(SessionListFailure.gone(gone).kind == "device_list_reset")
    }
}
