// R-IOS-9: the sign-in screen's rules, against a stubbed sign-in (no
// registry, no Keychain): which pages open, what the phone calls itself,
// and what each ending says. The same model against the fake fleet and the
// Keychain is AppTests' SignInTests.
import Foundation
import Synchronization
import Testing
@testable import KrowkKit

@Suite("R-IOS-9: the sign-in screen, against a stubbed sign-in")
@MainActor
struct SignInModelTests {
    @Test(
        "R-IOS-9: an ftp:, a javascript: or a hostless page, and http against an https registry, are refused and never handed to the presenter",
        arguments: [
            ("ftp://evil.example/approve", "https://api.krowk.com/v1"),
            ("javascript:alert(1)", "https://api.krowk.com/v1"),
            ("JavaScript://api.krowk.com/%0Aalert(1)", "https://api.krowk.com/v1"),
            ("http://api.krowk.com/approve?code=A", "https://api.krowk.com/v1"),
            ("https:///approve", "https://api.krowk.com/v1"),
            ("file:///etc/passwd", "http://127.0.0.1:9/v1"),
            ("krowk://approve", "http://127.0.0.1:9/v1"),
        ]
    )
    func unsafePagesAreRefused(page: String, api: String) async throws {
        #expect(SignInRules.browsablePage(page, api: api) == nil)
        let attempt = StubAttempt(page: page)
        let presenter = RecordingPresenter()
        let model = SignInModel(service: StubService(api: api, attempt: attempt), presenter: presenter, deviceName: "iPhone")
        model.start()
        try await until { model.phase == .failed(.unsafePage) }
        #expect(presenter.presented.isEmpty)
        #expect(attempt.cancelled)
        #expect(model.phase == .failed(.unsafePage))
    }

    @Test("R-IOS-9: https opens, and plain http only when the registry's own address is http")
    func browsablePages() {
        #expect(SignInRules.browsablePage("https://krowk.com/cli/authorizations/new?code=A", api: "https://api.krowk.com/v1") != nil)
        #expect(SignInRules.browsablePage("http://127.0.0.1:5/_approve/cli/authorizations/new?code=A", api: "http://127.0.0.1:5/v1") != nil)
        #expect(SignInRules.browsablePage("https://127.0.0.1:5/x", api: "http://127.0.0.1:5/v1") != nil)
    }

    @Test("R-IOS-9: the device name loses its control characters and is cut to 64 characters")
    func deviceName() {
        #expect(SignInRules.deviceName("Elvinas\u{0}'s\u{7} iPhone\n") == "Elvinas's iPhone")
        #expect(SignInRules.deviceName(String(repeating: "é", count: 80)) == String(repeating: "é", count: 64))
        #expect(SignInRules.deviceName("\u{1}\u{2}") == "iPhone")
        #expect(SignInRules.deviceName("iPhone") == "iPhone")
    }

    @Test("R-IOS-9: a DEBUG build takes KROWK_API_URL from the launch environment; otherwise krowk's registry")
    func apiURL() {
        #expect(SignInRules.apiURL(environment: [:]) == "https://api.krowk.com/v1")
        #expect(SignInRules.apiURL(environment: ["KROWK_API_URL": ""]) == "https://api.krowk.com/v1")
        #if DEBUG
        #expect(SignInRules.apiURL(environment: ["KROWK_API_URL": "http://127.0.0.1:9/v1"]) == "http://127.0.0.1:9/v1")
        #else
        #expect(SignInRules.apiURL(environment: ["KROWK_API_URL": "http://127.0.0.1:9/v1"]) == "https://api.krowk.com/v1")
        #endif
    }

    @Test("R-IOS-9: approval closes the sheet and moves on, naming the workspace")
    func approved() async throws {
        let attempt = StubAttempt(page: "https://krowk.com/cli/authorizations/new?code=K7QF-9M3X")
        let presenter = RecordingPresenter()
        let landed = Box<SignedInWorkspace?>(nil)
        let model = SignInModel(
            service: StubService(api: "https://api.krowk.com/v1", attempt: attempt),
            presenter: presenter, deviceName: "Test iPhone", onSignedIn: { landed.value = $0 })
        model.start()
        try await until { model.phase == .waiting(code: "K7QF-9M3X", sheetOpen: true) }
        #expect(presenter.presented == [URL(string: "https://krowk.com/cli/authorizations/new?code=K7QF-9M3X")!])
        attempt.finish(.success(SignedInWorkspace(workspace: "ws_1", name: "Acme")))
        try await until { landed.value != nil }
        #expect(model.phase == .signedIn(SignedInWorkspace(workspace: "ws_1", name: "Acme")))
        #expect(landed.value?.displayName == "Acme")
        #expect(presenter.dismissed > 0)
    }

    @Test("R-IOS-9: an expired sign-in says so in one sentence, and Start again opens a new one")
    func expired() async throws {
        let attempt = StubAttempt(page: "https://krowk.com/p?code=A")
        let presenter = RecordingPresenter()
        let model = SignInModel(service: StubService(api: "https://api.krowk.com/v1", attempt: attempt), presenter: presenter, deviceName: "iPhone")
        model.start()
        try await until { if case .waiting = model.phase { true } else { false } }
        attempt.finish(.failure(.expired))
        try await until { model.phase == .failed(.expired) }
        #expect(SignInFailure.expired.sentence == "The sign-in page expired after 15 minutes. Start again.")
        attempt.reset()
        model.start()
        try await until { if case .waiting = model.phase { true } else { false } }
        #expect(presenter.presented.count == 2)
        model.cancel()
    }

    @Test("R-IOS-9: a denial, a refused Keychain write: one sentence each, the Keychain's status only in the log")
    func sentences() async throws {
        #expect(SignInFailure.denied.sentence == "You said no in the browser. Start again to sign in.")
        #expect(SignInFailure.keychainRefused(detail: "OSStatus -25308").sentence == "This iPhone couldn't keep your sign-in safely.")
        let attempt = StubAttempt(page: "https://krowk.com/p?code=A")
        let logged = Box<[String]>([])
        let model = SignInModel(
            service: StubService(api: "https://api.krowk.com/v1", attempt: attempt), presenter: RecordingPresenter(),
            deviceName: "iPhone", log: { line in logged.value.append(line) })
        model.start()
        try await until { if case .waiting = model.phase { true } else { false } }
        attempt.finish(.failure(.keychainRefused(detail: "OSStatus -25308")))
        try await until { model.phase == .failed(.keychainRefused(detail: "OSStatus -25308")) }
        #expect(logged.value.contains { $0.contains("-25308") })
    }

    @Test("R-IOS-9: closing the sheet leaves the poll running; the page opens again; Cancel ends the poll")
    func sheetClosedEarly() async throws {
        let attempt = StubAttempt(page: "https://krowk.com/p?code=A")
        let presenter = RecordingPresenter()
        let model = SignInModel(service: StubService(api: "https://api.krowk.com/v1", attempt: attempt), presenter: presenter, deviceName: "iPhone")
        model.start()
        try await until { if case .waiting(_, true) = model.phase { true } else { false } }
        presenter.closeByPerson()
        #expect(model.phase == .waiting(code: attempt.code, sheetOpen: false))
        #expect(!attempt.cancelled)
        model.openPageAgain()
        #expect(model.phase == .waiting(code: attempt.code, sheetOpen: true))
        #expect(presenter.presented.count == 2)
        model.cancel()
        #expect(model.phase == .ready)
        #expect(attempt.cancelled)
        // A late answer for the cancelled run changes nothing.
        attempt.finish(.success(SignedInWorkspace(workspace: "ws_1", name: "Acme")))
        try await Task.sleep(for: .milliseconds(100))
        #expect(model.phase == .ready)
    }

    @Test("R-IOS-9: a model that goes away cancels its sign-in")
    func releasedModelCancels() async throws {
        let attempt = StubAttempt(page: "https://krowk.com/p?code=A")
        var model: SignInModel? = SignInModel(
            service: StubService(api: "https://api.krowk.com/v1", attempt: attempt), presenter: RecordingPresenter(), deviceName: "iPhone")
        model?.start()
        try await until { if case .waiting = model?.phase { true } else { false } }
        model = nil
        try await until { attempt.cancelled }
    }
}

/// A value the model's callbacks set and the test reads, behind a lock.
final class Box<Value: Sendable>: Sendable {
    private let lock: Mutex<Value>

    init(_ value: Value) {
        lock = Mutex(value)
    }

    var value: Value {
        get { lock.withLock { $0 } }
        set { lock.withLock { $0 = newValue } }
    }
}

/// Waits up to five seconds for `condition`, letting the model's tasks run.
@MainActor
func until(_ condition: @MainActor () -> Bool, sourceLocation: SourceLocation = #_sourceLocation) async throws {
    let deadline = ContinuousClock.now + .seconds(5)
    while !condition() {
        guard ContinuousClock.now < deadline else {
            Issue.record("timed out", sourceLocation: sourceLocation)
            return
        }
        try await Task.sleep(for: .milliseconds(10))
    }
}

struct StubService: SignInService {
    let api: String
    let attempt: StubAttempt

    func start(deviceName: String) throws(SignInFailure) -> any SignInAttempt {
        attempt
    }
}

/// A sign-in whose `wait` blocks until the test finishes it or it is cancelled.
final class StubAttempt: SignInAttempt {
    let code = "K7QF-9M3X"
    let verificationURL: String
    private let state = Mutex<(result: Result<SignedInWorkspace, SignInFailure>?, cancelled: Bool)>((nil, false))

    init(page: String) {
        verificationURL = page
    }

    var cancelled: Bool { state.withLock { $0.cancelled } }

    func finish(_ result: Result<SignedInWorkspace, SignInFailure>) {
        state.withLock { $0.result = result }
    }

    func reset() {
        state.withLock { $0 = (nil, false) }
    }

    func wait() throws(SignInFailure) -> SignedInWorkspace {
        while true {
            let (result, cancelled) = state.withLock { s in
                defer { s.result = nil }
                return (s.result, s.cancelled)
            }
            if cancelled { throw .cancelled }
            if let result { return try result.get() }
            Thread.sleep(forTimeInterval: 0.01)
        }
    }

    func cancel() {
        state.withLock { $0.cancelled = true }
    }
}

/// The browser, played by the test: it records what it was asked to open.
@MainActor
final class RecordingPresenter: WebAuthPresenter {
    private(set) var presented: [URL] = []
    private(set) var dismissed = 0
    private var onClose: (@MainActor () -> Void)?

    func present(_ page: URL, onClose: @escaping @MainActor () -> Void) {
        presented.append(page)
        self.onClose = onClose
    }

    func dismiss() {
        dismissed += 1
        onClose = nil
    }

    /// The person swipes the sheet away.
    func closeByPerson() {
        let close = onClose
        onClose = nil
        close?()
    }
}
