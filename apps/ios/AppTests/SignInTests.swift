// R-IOS-9, R-CLIENT-1, R-CRED-1: the iPhone signs in through the registry's
// browser approval, against the fake fleet's stand-in registry: the sign-in
// screen's model, krowk-ffi's SignIn under it, and the key landing in the
// Keychain through KeychainKeyStorage. The test plays the browser: the
// presenter only records the page, and the test approves or denies the code
// with the stand-in's /_approve endpoints, as the page's buttons would.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh starts it and hands its
// API URL to this process as KROWK_API_URL (TEST_RUNNER_KROWK_API_URL).
// Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKeys
import KrowkKit
import Synchronization
import Testing

extension HostedKeychain {
    @Suite("R-IOS-9: the iPhone signs in through the fake fleet's browser approval, its key kept in the Keychain")
    @MainActor
    struct SignInTests {
        /// The fleet's `/v1` URL, and the stand-in's site it hangs off.
        let api: String
        let site: String
        let storage: KeychainKeyStorage

        /// What the phone calls itself here, a control character and all:
        /// the registry gets it without one.
        static let deviceName = "Fleet\u{7} iPhone"
        static let sentName = "Fleet iPhone"

        init() throws {
            let api = try #require(
                ProcessInfo.processInfo.environment["KROWK_API_URL"].flatMap { $0.isEmpty ? nil : $0 },
                "KROWK_API_URL is unset: run the tests through apps/ios/scripts/with-fleet.sh, which starts the fake fleet"
            )
            self.api = api
            site = api.hasSuffix("/v1") ? String(api.dropLast(3)) : api
            // A first launch: the storage starts from an empty service.
            storage = try KeychainKeyStorage(
                installMarker: URL.applicationSupportDirectory.appending(path: "krowk-tests/\(UUID().uuidString).installed"))
        }

        @Test("R-IOS-9, R-CLIENT-1, R-CRED-1: start, approve through the stand-in, and the key is in the Keychain; the screen names the workspace")
        func approved() async throws {
            let browser = Browser()
            let landed = Landed()
            let model = SignInModel(
                service: RegistrySignIn(api: api, storage: storage), presenter: browser,
                deviceName: Self.deviceName, onSignedIn: { landed.set($0) })
            model.start()
            let code = try await waitingCode(model)

            // The page the presenter got is the stand-in's, for this code.
            let page = try #require(browser.presented.first)
            #expect(page.absoluteString.hasPrefix("\(site)/_approve/cli/authorizations/new?code="), "\(page)")
            #expect(page.absoluteString.hasSuffix(code), "\(page)")
            // R-IOS-9: the start request carried machine = the device's
            // name, cleaned; the approval page shows it.
            let html = try await get(page)
            #expect(html.contains("A terminal on \(Self.sentName) asked to sign in"), "\(html)")

            try await post("/_approve/cli/authorizations/\(code)/approval")
            try await eventually(seconds: 30) { if case .signedIn = model.phase { true } else { false } }
            guard case .signedIn(let workspace) = model.phase else {
                Issue.record("not signed in: \(model.phase)")
                return
            }
            #expect(!workspace.name.isEmpty && !workspace.workspace.isEmpty, "\(workspace)")
            #expect(landed.get() == workspace)
            #expect(browser.dismissed > 0, "the sheet was closed when the poll reported the approval")

            // The key is in the Keychain item credentials.json, with its
            // workspace; and only krowk's own key (R-CRED-1).
            let saved = try #require(try storage.read(slot: "credentials.json"))
            let creds = try #require(try JSONSerialization.jsonObject(with: saved) as? [String: Any])
            #expect((creds["token"] as? String)?.isEmpty == false)
            #expect(creds["workspace"] as? String == workspace.workspace)
            #expect(creds["workspace_name"] as? String == workspace.name)
            #expect(Set(creds.keys) == ["version", "token", "key_id", "workspace", "workspace_name"])

            // The next launch opens on "Signed in to <workspace>", not onboarding.
            #expect(try Launch.read(api: api, storage: storage) == .signedIn(workspace))
        }

        @Test("R-IOS-9: after sign-in no file in the app's container and no UserDefaults key holds the key")
        func keyIsOnlyInTheKeychain() async throws {
            let model = SignInModel(service: RegistrySignIn(api: api, storage: storage), presenter: Browser(), deviceName: Self.deviceName)
            model.start()
            let code = try await waitingCode(model)
            try await post("/_approve/cli/authorizations/\(code)/approval")
            try await eventually(seconds: 30) { if case .signedIn = model.phase { true } else { false } }

            let saved = try #require(try storage.read(slot: "credentials.json"))
            let creds = try #require(try JSONSerialization.jsonObject(with: saved) as? [String: Any])
            let token = try #require(creds["token"] as? String)
            try #require(token.count >= 16, "a key this short would match by chance")
            let needle = Data(token.utf8)

            // No UserDefaults key, in any domain the app reads.
            UserDefaults.standard.synchronize()
            for (key, value) in UserDefaults.standard.dictionaryRepresentation() {
                #expect(!String(describing: value).contains(token), "UserDefaults \(key) holds the key")
                if let data = value as? Data { #expect(data.range(of: needle) == nil, "UserDefaults \(key) holds the key") }
            }
            // No file in the container: Documents, Library (Preferences,
            // Caches, Application Support) and tmp.
            let (scanned, holding) = Self.files(holding: needle)
            #expect(scanned > 0, "no file scanned under \(NSHomeDirectory())")
            #expect(holding.isEmpty, "these files hold the key: \(holding)")
        }

        @Test("R-IOS-9: a denial says so in one sentence with Start again, and keeps nothing")
        func denied() async throws {
            let model = SignInModel(service: RegistrySignIn(api: api, storage: storage), presenter: Browser(), deviceName: Self.deviceName)
            model.start()
            let code = try await waitingCode(model)
            try await post("/_approve/cli/authorizations/\(code)/denial")
            try await eventually(seconds: 30) { model.phase == .failed(.denied) }
            #expect(SignInFailure.denied.sentence == "You said no in the browser. Start again to sign in.")
            #expect(try storage.read(slot: "credentials.json") == nil)
            #expect(try Launch.read(api: api, storage: storage) == .signedOut)

            // Start again opens a new sign-in, with a new code.
            model.start()
            let again = try await waitingCode(model)
            #expect(again != code)
            model.cancel()
            #expect(model.phase == .ready)
        }

        @Test("R-IOS-9: a Keychain that won't keep the key says so in one sentence, its status only in the log")
        func keychainRefuses() async throws {
            let lines = Lines()
            let model = SignInModel(
                service: RegistrySignIn(api: api, storage: Refusing()), presenter: Browser(),
                deviceName: Self.deviceName, log: { lines.append($0) })
            model.start()
            let code = try await waitingCode(model)
            try await post("/_approve/cli/authorizations/\(code)/approval")
            try await eventually(seconds: 30) { if case .failed(.keychainRefused) = model.phase { true } else { false } }
            guard case .failed(let failure) = model.phase else { return }
            #expect(failure.sentence == "This iPhone couldn't keep your sign-in safely.")
            #expect(!failure.sentence.contains("-25308"))
            #expect(lines.all().contains { $0.contains("-25308") }, "\(lines.all())")
        }

        // MARK: Helpers

        /// Every regular file in the app's container, and those of them
        /// whose bytes contain `needle`.
        private nonisolated static func files(holding needle: Data) -> (scanned: Int, holding: [String]) {
            let home = URL(fileURLWithPath: NSHomeDirectory())
            guard let files = FileManager.default.enumerator(at: home, includingPropertiesForKeys: [.isRegularFileKey]) else {
                return (0, [])
            }
            var scanned = 0
            var holding: [String] = []
            for case let file as URL in files {
                guard (try? file.resourceValues(forKeys: [.isRegularFileKey]))?.isRegularFile == true,
                      let contents = try? Data(contentsOf: file, options: .alwaysMapped)
                else { continue }
                scanned += 1
                if contents.range(of: needle) != nil { holding.append(file.path) }
            }
            return (scanned, holding)
        }

        /// Waits for the model to show its code, the page open.
        private func waitingCode(_ model: SignInModel) async throws -> String {
            try await eventually(seconds: 30) { if case .waiting = model.phase { true } else { false } }
            guard case .waiting(let code, _) = model.phase else {
                throw Unexpected(what: "no code: \(model.phase)")
            }
            return code
        }

        private func get(_ url: URL) async throws -> String {
            let (body, response) = try await URLSession.shared.data(from: url)
            #expect((response as? HTTPURLResponse)?.statusCode == 200, "GET \(url)")
            return String(decoding: body, as: UTF8.self)
        }

        /// What the approval page's buttons post.
        private func post(_ path: String) async throws {
            var request = URLRequest(url: try #require(URL(string: site + path)))
            request.httpMethod = "POST"
            let (_, response) = try await URLSession.shared.data(for: request)
            #expect((response as? HTTPURLResponse)?.statusCode == 200, "POST \(path)")
        }
    }
}

struct Unexpected: Error {
    let what: String
}

/// Waits for `condition`, letting the model's tasks run; records an issue
/// and throws when it never holds.
@MainActor
func eventually(seconds: Int, _ condition: @MainActor () -> Bool, sourceLocation: SourceLocation = #_sourceLocation) async throws {
    let deadline = ContinuousClock.now + .seconds(seconds)
    while !condition() {
        guard ContinuousClock.now < deadline else {
            Issue.record("still not so after \(seconds) s", sourceLocation: sourceLocation)
            throw Unexpected(what: "timed out")
        }
        try await Task.sleep(for: .milliseconds(50))
    }
}

/// The browser, played by the test: records the pages it was asked to open.
@MainActor
final class Browser: WebAuthPresenter {
    private(set) var presented: [URL] = []
    private(set) var dismissed = 0

    func present(_ page: URL, onClose: @escaping @MainActor () -> Void) {
        presented.append(page)
    }

    func dismiss() {
        dismissed += 1
    }
}

final class Landed: Sendable {
    private let value = Mutex<SignedInWorkspace?>(nil)
    func set(_ new: SignedInWorkspace) { value.withLock { $0 = new } }
    func get() -> SignedInWorkspace? { value.withLock { $0 } }
}

final class Lines: Sendable {
    private let lines = Mutex<[String]>([])
    func append(_ line: String) { lines.withLock { $0.append(line) } }
    func all() -> [String] { lines.withLock { $0 } }
}

/// A Keychain that refuses every write, as a locked phone's does.
final class Refusing: KeyStorage {
    func read(slot: String) throws -> Data? { nil }
    func write(slot: String, bytes: Data) throws {
        throw KrowkError.Internal(detail: "OSStatus -25308 (errSecInteractionNotAllowed) writing \(slot)")
    }
    func remove(slot: String) throws {}
}
