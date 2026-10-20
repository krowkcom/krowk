// R-IOS-3, R-CLIENT-1: the session list and opening a session are the Rust
// core's, reached through Account; Swift sees the list's records and the
// relay the phone uses, and never a key. No registry is called: the core
// refuses an unpaired phone before it asks one, and picks the relay from
// the registry's address alone.
import Foundation
import Krowk
import Testing

/// Keychain items in memory.
private final class MemoryStorage: KeyStorage, @unchecked Sendable {
    private let lock = NSLock()
    private var items: [String: Data] = [:]

    func read(slot: String) throws -> Data? {
        lock.withLock { items[slot] }
    }

    func write(slot: String, bytes: Data) throws {
        lock.withLock { items[slot] = bytes }
    }

    func remove(slot: String) throws {
        lock.withLock { _ = items.removeValue(forKey: slot) }
    }
}

/// A phone signed in to krowk's own registry and not paired yet.
private func signedIn() throws -> Account {
    let storage = MemoryStorage()
    let creds = #"{"version":1,"token":"krowk_sk_swift_sessions","key_id":"key_1","workspace":"ws","workspace_name":"W"}"#
    try storage.write(slot: "credentials.json", bytes: Data(creds.utf8))
    return try Account.open(api: "https://api.krowk.com/v1", storage: storage)
}

@Suite("R-IOS-3: the session list and attach cross to Swift")
struct SessionsTests {
    @Test("R-IOS-3: listSessions() and openSession(id:known:relayOverride:) refuse an unpaired phone with not_paired")
    func unpairedIsRefused() throws {
        let account = try signedIn()
        do {
            let list = try account.listSessions()
            Issue.record("listed \(list.sessions.count) sessions unpaired")
        } catch KrowkError.Refused(let code, _) {
            #expect(code == "not_paired")
        }
        let known = HeadRecord(index: 3, digest: Data(repeating: 0, count: 32))
        do {
            _ = try account.openSession(id: "01a0ec7b-3333-7000-8000-000000000079", known: known, relayOverride: nil)
            Issue.record("opened a session unpaired")
        } catch KrowkError.Refused(let code, _) {
            #expect(code == "not_paired")
        }
    }

    @Test("R-CLIENT-1: relayUrl(relayOverride:) is krowk's hosted relay for krowk's registry, unless the app names one")
    func relayFollowsTheRegistry() throws {
        let account = try signedIn()
        #expect(account.relayUrl(relayOverride: nil) == "wss://relay.krowk.com")
        #expect(account.relayUrl(relayOverride: "ws://127.0.0.1:7790") == "ws://127.0.0.1:7790")
        #expect(["production", "development"].contains(account.relayEnv()))
    }

    @Test("R-IOS-3: a SessionSummary carries what the list screen shows")
    func summaryShape() {
        let s = SessionSummary(
            id: "s", title: "fix the build", cwd: "/src", updatedMs: 7,
            head: HeadRecord(index: 2, digest: Data(count: 32)), hostedBy: nil, hostDevice: "d")
        let list = SessionList(sessions: [s], unreadable: 1)
        #expect(list.sessions.first?.head?.index == 2)
        #expect(list.unreadable == 1)
    }
}
