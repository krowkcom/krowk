// R-IOS-8, R-E2E-2, R-E2E-3, R-CLIENT-1: on the iPhone every slot krowk
// keeps is a Keychain item, sealed under the install's wrapping key, and
// the Rust keystore runs on it unchanged. On the simulator the wrapping key
// is a software key in the same Keychain item (there is no Secure Enclave);
// everything else is what a device runs.
import Foundation
import Krowk
import Security
import Testing
@testable import KrowkKeys

/// Every test starts from an empty `com.krowk.keys` service and a fresh
/// first launch; they share the one Keychain, so they run one at a time.
@Suite("R-IOS-8: KeychainKeyStorage keeps krowk's slots in the Keychain, sealed", .serialized)
struct KeychainKeyStorageTests {
    let defaults: UserDefaults
    let storage: KeychainKeyStorage

    init() throws {
        defaults = Self.freshDefaults()
        storage = try KeychainKeyStorage(defaults: defaults)
    }

    @Test("R-IOS-8: write then read round-trips a slot's bytes")
    func roundTrips() throws {
        let bytes = Self.random(64)
        try storage.write(slot: "device.json", bytes: bytes)
        #expect(try storage.read(slot: "device.json") == bytes)
        // Empty bytes are bytes, not absence.
        try storage.write(slot: "user-keys.json", bytes: Data())
        #expect(try storage.read(slot: "user-keys.json") == Data())
    }

    @Test("R-IOS-8: a slot never written reads as absent")
    func missingIsAbsent() throws {
        #expect(try storage.read(slot: "signing.json") == nil)
    }

    @Test("R-IOS-8: remove is idempotent, and a removed slot reads as absent")
    func removeIsIdempotent() throws {
        try storage.remove(slot: "device-list.json")
        try storage.write(slot: "device-list.json", bytes: Self.random(40))
        try storage.remove(slot: "device-list.json")
        try storage.remove(slot: "device-list.json")
        #expect(try storage.read(slot: "device-list.json") == nil)
    }

    @Test("R-IOS-8: a second write replaces the slot's bytes")
    func overwriteReplaces() throws {
        try storage.write(slot: "credentials.json", bytes: Self.random(48))
        let second = Self.random(80)
        try storage.write(slot: "credentials.json", bytes: second)
        #expect(try storage.read(slot: "credentials.json") == second)
        #expect(try Self.accounts() == ["credentials.json", "wrapping-key"])
    }

    @Test("R-IOS-8: an item sealed under one slot name does not open under another (authenticated data)")
    func sealedUnderOneNameOnly() throws {
        try storage.write(slot: "device.json", bytes: Self.random(64))
        let sealed = try #require(try Keychain.read("device.json"))
        try Keychain.write("signing.json", sealed, replace: true)
        Self.expectInternal { _ = try storage.read(slot: "signing.json") }
        // The original still opens.
        #expect(try storage.read(slot: "device.json") != nil)
    }

    @Test("R-IOS-8: an item that is there but does not open is an error, never absent and never the stored bytes")
    func unreadableIsAnError() throws {
        try Keychain.write("device.json", Data("not sealed".utf8), replace: true)
        Self.expectInternal { _ = try storage.read(slot: "device.json") }
        // Sealed by another install's key: the same format, another key.
        try storage.write(slot: "signing.json", bytes: Self.random(64))
        let sealed = try #require(try Keychain.read("signing.json"))
        try Self.purge()
        let other = try KeychainKeyStorage(defaults: Self.freshDefaults())
        try Keychain.write("signing.json", sealed, replace: true)
        Self.expectInternal { _ = try other.read(slot: "signing.json") }
    }

    @Test("R-IOS-8: no slot may read, replace or remove the wrapping key's item")
    func wrappingKeyIsNotASlot() throws {
        let before = try #require(try Keychain.read(wrappingKeyAccount))
        for call in [
            { _ = try storage.read(slot: "wrapping-key") },
            { try storage.write(slot: "wrapping-key", bytes: Data([1])) },
            { try storage.remove(slot: "wrapping-key") },
            { _ = try storage.read(slot: "") },
        ] as [() throws -> Void] {
            #expect {
                try call()
            } throws: { error in
                if case KrowkError.InvalidInput = error { true } else { false }
            }
        }
        #expect(try Keychain.read(wrappingKeyAccount) == before)
    }

    @Test("R-IOS-8: every item is WhenUnlockedThisDeviceOnly, not synchronizable, under com.krowk.keys, and holds no plaintext")
    func itemAttributes() throws {
        let slots = ["device.json": Self.random(64), "signing.json": Self.random(64), "credentials.json": Self.random(90)]
        for (slot, bytes) in slots { try storage.write(slot: slot, bytes: bytes) }

        let items = try Self.items()
        #expect(Set(items.compactMap { $0[kSecAttrAccount as String] as? String }) == Set(slots.keys).union(["wrapping-key"]))
        for item in items {
            let account = item[kSecAttrAccount as String] as? String ?? "?"
            #expect(item[kSecAttrService as String] as? String == "com.krowk.keys", "\(account)")
            #expect(
                item[kSecAttrAccessible as String] as? String == kSecAttrAccessibleWhenUnlockedThisDeviceOnly as String,
                "\(account): \(String(describing: item[kSecAttrAccessible as String]))"
            )
            #expect((item[kSecAttrSynchronizable as String] as? NSNumber)?.boolValue != true, "\(account) is synchronizable")
            let stored = try #require(item[kSecValueData as String] as? Data, "\(account) has no data")
            if let plaintext = slots[account] {
                #expect(stored != plaintext, "\(account) holds its plaintext")
                #expect(stored.range(of: plaintext) == nil, "\(account) contains its plaintext")
                #expect(stored.first == KeychainKeyStorage.format)
                #expect(stored.count == 1 + 65 + 12 + plaintext.count + 16)
            }
        }
        // None of them synced: asking for synchronizable items finds none.
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "com.krowk.keys",
            kSecAttrSynchronizable as String: true,
        ]
        q[kSecMatchLimit as String] = kSecMatchLimitAll
        #expect(SecItemCopyMatching(q as CFDictionary, nil) == errSecItemNotFound)
    }

    @Test("R-IOS-8, R-E2E-2, R-E2E-3, R-CLIENT-1: the Rust keystore makes the device and signing keys through the FFI; a fresh KeychainKeyStorage reads the same device")
    func rustKeystoreOverTheKeychain() throws {
        try storage.write(slot: "credentials.json", bytes: Self.signedIn)
        let first = try Account.open(api: Self.api, storage: storage)
        #expect(first.deviceId().count == 32, "\(first.deviceId())")
        #expect(try Self.accounts() == ["credentials.json", "device.json", "signing.json", "wrapping-key"])
        #expect(try !first.hasUserKeys())

        // The keystore's own bytes are not in the Keychain item.
        for slot in ["device.json", "signing.json"] {
            let plaintext = try #require(try storage.read(slot: slot))
            let stored = try #require(try Keychain.read(slot))
            #expect(stored.range(of: plaintext) == nil, "\(slot) holds its plaintext")
        }

        // A later launch: the marker is set, so nothing is purged.
        let again = try KeychainKeyStorage(defaults: defaults)
        let second = try Account.open(api: Self.api, storage: again)
        #expect(second.deviceId() == first.deviceId())
    }

    @Test("R-IOS-8: the first launch after an install deletes every item an earlier install left, then sets krowk.installed")
    func firstLaunchPurges() throws {
        try storage.write(slot: "device.json", bytes: Self.random(64))
        try storage.write(slot: "credentials.json", bytes: Self.signedIn)
        let oldWrappingKey = try Keychain.read(wrappingKeyAccount)
        #expect(defaults.bool(forKey: KeychainKeyStorage.installedMarker))

        // The same install launching again keeps them.
        let relaunch = try KeychainKeyStorage(defaults: defaults)
        #expect(try relaunch.read(slot: "device.json") != nil)

        // A reinstall: the app's UserDefaults went with the app; the
        // Keychain items did not.
        let reinstalled = Self.freshDefaults()
        let fresh = try KeychainKeyStorage(defaults: reinstalled)
        #expect(try fresh.read(slot: "device.json") == nil)
        #expect(try fresh.read(slot: "credentials.json") == nil)
        #expect(try Self.accounts() == ["wrapping-key"])
        #expect(try Keychain.read(wrappingKeyAccount) != oldWrappingKey, "the earlier install's wrapping key was kept")
        #expect(reinstalled.bool(forKey: KeychainKeyStorage.installedMarker))
    }

    @Test("R-IOS-8: after keys are made, no file in the app's Documents, Library or tmp holds any slot's bytes")
    func noKeyFileInTheContainer() throws {
        try storage.write(slot: "credentials.json", bytes: Self.signedIn)
        _ = try Account.open(api: Self.api, storage: storage)
        let extra = Self.random(64)
        try storage.write(slot: "user-keys.json", bytes: extra)
        var secrets: [String: Data] = [:]
        for slot in ["device.json", "signing.json", "user-keys.json"] {
            let bytes: Data = try #require(try storage.read(slot: slot))
            secrets[slot] = bytes
        }

        let home = URL(fileURLWithPath: NSHomeDirectory())
        let roots = [
            home.appendingPathComponent("Documents"), home.appendingPathComponent("Library"),
            home.appendingPathComponent("tmp"), FileManager.default.temporaryDirectory,
        ]
        var scanned = 0
        for root in roots {
            guard let files = FileManager.default.enumerator(at: root, includingPropertiesForKeys: [.isRegularFileKey, .fileSizeKey])
            else { continue }
            for case let file as URL in files {
                let values = try? file.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey])
                guard values?.isRegularFile == true, let contents = try? Data(contentsOf: file, options: .alwaysMapped) else { continue }
                scanned += 1
                for (slot, bytes) in secrets {
                    #expect(contents.range(of: bytes) == nil, "\(file.path) holds \(slot)'s bytes")
                }
            }
        }
        print("scanned \(scanned) files under \(home.path)")
    }

    // MARK: Helpers

    static let api = "http://127.0.0.1:9/v1"
    /// A krowk API key, as sign-in stores it; never sent anywhere here.
    static let signedIn = Data(#"{"version":1,"token":"krowk_sk_test","key_id":"key_1","workspace":"ws_1","workspace_name":"Test"}"#.utf8)

    /// A first launch: no marker, so the storage purges the service.
    static func freshDefaults() -> UserDefaults {
        let name = "krowk.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: name)!
        defaults.removePersistentDomain(forName: name)
        return defaults
    }

    static func purge() throws {
        try Keychain.removeAll()
    }

    static func random(_ count: Int) -> Data {
        Data((0..<count).map { _ in UInt8.random(in: 0...255) })
    }

    /// Every item under the service, attributes and data.
    static func items() throws -> [[String: Any]] {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "com.krowk.keys",
            kSecAttrSynchronizable as String: kSecAttrSynchronizableAny,
            kSecMatchLimit as String: kSecMatchLimitAll,
            kSecReturnAttributes as String: true,
            kSecReturnData as String: true,
        ]
        var out: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        if status == errSecItemNotFound { return [] }
        try #require(status == errSecSuccess, "SecItemCopyMatching: OSStatus \(status)")
        return out as? [[String: Any]] ?? []
    }

    static func accounts() throws -> [String] {
        try items().compactMap { $0[kSecAttrAccount as String] as? String }.sorted()
    }

    static func expectInternal(_ call: () throws -> Void) {
        #expect {
            try call()
        } throws: { error in
            if case KrowkError.Internal(let detail) = error { !detail.isEmpty } else { false }
        }
    }
}
