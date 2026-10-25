// R-IOS-8, R-E2E-2, R-E2E-3, R-CLIENT-1: on the iPhone every slot krowk
// keeps is a Keychain item, sealed under the install's wrapping key, and
// the Rust keystore runs on it unchanged. On the simulator the wrapping key
// is a software key in the same Keychain item (there is no Secure Enclave);
// everything else is what a device runs. Hosted in the app, whose
// application identifier the Keychain needs; the tests look at the items
// through the Security framework directly, not through KrowkKeys.
import Foundation
import Krowk
import KrowkKeys
import Security
import Testing

extension HostedKeychain {
    /// Every test starts from an empty `com.krowk.keys` service and a fresh
    /// first launch; they share the one Keychain, so they run one at a time.
    @Suite("R-IOS-8: KeychainKeyStorage keeps krowk's slots in the Keychain, sealed", .serialized)
    struct KeychainKeyStorageTests {
        let marker: URL
        let storage: KeychainKeyStorage

        init() throws {
            marker = Self.freshMarker()
            storage = try KeychainKeyStorage(installMarker: marker)
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

        @Test("R-IOS-8, R-E2E-3: forgetAll deletes every item, the wrapping key's too, and the storage refuses every call after it")
        func forgetAllForgetsEverything() throws {
            try storage.write(slot: "device.json", bytes: Self.random(64))
            try storage.write(slot: "credentials.json", bytes: Self.random(48))
            #expect(try Self.accounts() == ["credentials.json", "device.json", "wrapping-key"])
            try storage.forgetAll()
            #expect(try Self.accounts() == [])
            // Nothing lands after: a write would be sealed to a key that is gone.
            Self.expectInternal { try storage.write(slot: "device.json", bytes: Self.random(64)) }
            Self.expectInternal { try storage.remove(slot: "device.json") }
            Self.expectInternal { _ = try storage.read(slot: "credentials.json") }
            #expect(try Self.accounts() == [])
            // The next storage is no first launch, finds nothing, and makes
            // a wrapping key of its own.
            let next = try KeychainKeyStorage(installMarker: marker)
            #expect(try next.read(slot: "device.json") == nil)
            try next.write(slot: "device.json", bytes: Data([1]))
            #expect(try next.read(slot: "device.json") == Data([1]))
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
            let sealed = try #require(try Self.stored("device.json"))
            try Self.store("signing.json", sealed)
            Self.expectInternal { _ = try storage.read(slot: "signing.json") }
            // The original still opens.
            #expect(try storage.read(slot: "device.json") != nil)
        }

        @Test("R-IOS-8: an item that is there but does not open is an error, never absent and never the stored bytes")
        func unreadableIsAnError() throws {
            try Self.store("device.json", Data("not sealed".utf8))
            Self.expectInternal { _ = try storage.read(slot: "device.json") }
            // Sealed by another install's key: the same format, another key.
            try storage.write(slot: "signing.json", bytes: Self.random(64))
            let sealed = try #require(try Self.stored("signing.json"))
            let other = try KeychainKeyStorage(installMarker: Self.freshMarker())
            try Self.store("signing.json", sealed)
            Self.expectInternal { _ = try other.read(slot: "signing.json") }
        }

        @Test("R-IOS-8: no slot may read, replace or remove the wrapping key's item")
        func wrappingKeyIsNotASlot() throws {
            let before = try #require(try Self.stored("wrapping-key"))
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
            #expect(try Self.stored("wrapping-key") == before)
        }

        @Test("R-IOS-8: every item is WhenUnlockedThisDeviceOnly, not synchronizable, under com.krowk.keys, and holds no plaintext")
        func itemAttributes() throws {
            let slots = ["device.json": Self.random(64), "signing.json": Self.random(64), "credentials.json": Self.random(90)]
            for (slot, bytes) in slots { try storage.write(slot: slot, bytes: bytes) }
            // A second write goes through SecItemUpdate: it must keep them too.
            try storage.write(slot: "device.json", bytes: slots["device.json"]!)

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
                    // 1 ‖ ephemeral key ‖ nonce ‖ ciphertext ‖ tag.
                    #expect(stored.first == 1)
                    #expect(stored.count == 1 + 65 + 12 + plaintext.count + 16)
                }
            }
            // None of them synced: asking for synchronizable items finds none.
            let synced: [String: Any] = [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: "com.krowk.keys",
                kSecAttrSynchronizable as String: true,
                kSecMatchLimit as String: kSecMatchLimitAll,
            ]
            #expect(SecItemCopyMatching(synced as CFDictionary, nil) == errSecItemNotFound)
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
                let stored = try #require(try Self.stored(slot))
                #expect(stored.range(of: plaintext) == nil, "\(slot) holds its plaintext")
            }

            // A later launch: the marker is there, so nothing is purged.
            let again = try KeychainKeyStorage(installMarker: marker)
            let second = try Account.open(api: Self.api, storage: again)
            #expect(second.deviceId() == first.deviceId())
        }

        @Test("R-IOS-8: the first launch after an install deletes every item an earlier install left, then writes its marker, kept out of backups")
        func firstLaunchPurges() throws {
            try storage.write(slot: "device.json", bytes: Self.random(64))
            try storage.write(slot: "credentials.json", bytes: Self.signedIn)
            let oldWrappingKey = try Self.stored("wrapping-key")
            #expect(FileManager.default.fileExists(atPath: marker.path(percentEncoded: false)))
            #expect(try marker.resourceValues(forKeys: [.isExcludedFromBackupKey]).isExcludedFromBackup == true)

            // The same install launching again keeps them.
            let relaunch = try KeychainKeyStorage(installMarker: marker)
            #expect(try relaunch.read(slot: "device.json") != nil)

            // A reinstall, or a restore from a backup: the marker went with the
            // app's container (or was never in the backup); the Keychain items
            // did not.
            let reinstalled = Self.freshMarker()
            let fresh = try KeychainKeyStorage(installMarker: reinstalled)
            #expect(try fresh.read(slot: "device.json") == nil)
            #expect(try fresh.read(slot: "credentials.json") == nil)
            #expect(try Self.accounts() == ["wrapping-key"])
            #expect(try Self.stored("wrapping-key") != oldWrappingKey, "the earlier install's wrapping key was kept")
            #expect(FileManager.default.fileExists(atPath: reinstalled.path(percentEncoded: false)))
        }

        @Test("R-IOS-1, R-IOS-8: the first launch after an install deletes the session cache an earlier one left, in the same purge; a later launch keeps it")
        func firstLaunchPurgesTheCache() throws {
            let cache = URL.temporaryDirectory.appending(path: "krowk-cache-\(UUID().uuidString).sqlite")
            let side = URL(filePath: cache.path(percentEncoded: false) + "-wal")
            for file in [cache, side] { try Data("left".utf8).write(to: file) }
            let reinstalled = Self.freshMarker()
            _ = try KeychainKeyStorage(installMarker: reinstalled, purging: [cache, side])
            #expect(!FileManager.default.fileExists(atPath: cache.path(percentEncoded: false)))
            #expect(!FileManager.default.fileExists(atPath: side.path(percentEncoded: false)))

            try Data("kept".utf8).write(to: cache)
            defer { try? FileManager.default.removeItem(at: cache) }
            _ = try KeychainKeyStorage(installMarker: reinstalled, purging: [cache, side])
            #expect(FileManager.default.fileExists(atPath: cache.path(percentEncoded: false)))
        }

        @Test("R-IOS-8: slots left without their wrapping key, which can never open, are cleared when a new key is made")
        func orphanedSlotsAreCleared() throws {
            try storage.write(slot: "device.json", bytes: Self.random(64))
            SecItemDelete(Self.query("wrapping-key") as CFDictionary)
            let reopened = try KeychainKeyStorage(installMarker: marker)
            #expect(try reopened.read(slot: "device.json") == nil)
            #expect(try Self.accounts() == ["wrapping-key"])
        }

        @Test("R-IOS-8: after keys are made, no file in the app's container (Documents, Library, tmp) holds any slot's bytes")
        func noKeyFileInTheContainer() throws {
            try storage.write(slot: "credentials.json", bytes: Self.signedIn)
            _ = try Account.open(api: Self.api, storage: storage)
            try storage.write(slot: "user-keys.json", bytes: Self.random(64))
            var secrets: [String: Data] = [:]
            for slot in ["device.json", "signing.json", "user-keys.json", "credentials.json"] {
                let bytes: Data = try #require(try storage.read(slot: slot))
                secrets[slot] = bytes
                // And each key inside it on its own, as hex and as raw bytes,
                // in case a file held it in some other wrapping.
                guard slot == "device.json" || slot == "signing.json",
                      let json = try JSONSerialization.jsonObject(with: bytes) as? [String: Any]
                else { continue }
                for (field, value) in json {
                    guard let hex = value as? String, hex.count >= 64, let raw = Self.unhex(hex) else { continue }
                    secrets["\(slot) \(field) (hex)"] = Data(hex.utf8)
                    secrets["\(slot) \(field)"] = raw
                }
            }
            #expect(secrets.count > 4, "no key field found in device.json or signing.json: \(secrets.keys.sorted())")

            // The whole container, which holds Documents, Library and tmp.
            let home = URL(fileURLWithPath: NSHomeDirectory())
            for part in ["Documents", "Library", "tmp"] {
                #expect(FileManager.default.fileExists(atPath: home.appendingPathComponent(part).path), "no \(part) in \(home.path)")
            }
            let files = try #require(FileManager.default.enumerator(at: home, includingPropertiesForKeys: [.isRegularFileKey]))
            var scanned = 0
            for case let file as URL in files {
                guard (try? file.resourceValues(forKeys: [.isRegularFileKey]))?.isRegularFile == true,
                      let contents = try? Data(contentsOf: file, options: .alwaysMapped)
                else { continue }
                scanned += 1
                for (slot, bytes) in secrets {
                    #expect(contents.range(of: bytes) == nil, "\(file.path) holds \(slot)'s bytes")
                }
            }
            // At least the install markers the tests wrote.
            #expect(scanned > 0, "no file scanned under \(home.path)")
        }

        // MARK: Helpers

        static let api = "http://127.0.0.1:9/v1"
        /// A krowk API key, as sign-in stores it; never sent anywhere here.
        static let signedIn = Data(#"{"version":1,"token":"krowk_sk_test","key_id":"key_1","workspace":"ws_1","workspace_name":"Test"}"#.utf8)

        /// A first launch: a marker that isn't there yet, so the storage purges
        /// the service.
        static func freshMarker() -> URL {
            URL.applicationSupportDirectory.appending(path: "krowk-tests/\(UUID().uuidString).installed")
        }

        static func unhex(_ hex: String) -> Data? {
            guard hex.count % 2 == 0 else { return nil }
            var out = Data(capacity: hex.count / 2)
            var index = hex.startIndex
            while index < hex.endIndex {
                let next = hex.index(index, offsetBy: 2)
                guard let byte = UInt8(hex[index..<next], radix: 16) else { return nil }
                out.append(byte)
                index = next
            }
            return out
        }

        static func random(_ count: Int) -> Data {
            Data((0..<count).map { _ in UInt8.random(in: 0...255) })
        }

        static func query(_ account: String) -> [String: Any] {
            [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: "com.krowk.keys",
                kSecAttrAccount as String: account,
                kSecUseDataProtectionKeychain as String: true,
            ]
        }

        /// The item's stored bytes, sealed: what the Keychain holds.
        static func stored(_ account: String) throws -> Data? {
            var q = query(account)
            q[kSecReturnData as String] = true
            var out: CFTypeRef?
            let status = SecItemCopyMatching(q as CFDictionary, &out)
            if status == errSecItemNotFound { return nil }
            try #require(status == errSecSuccess, "SecItemCopyMatching \(account): OSStatus \(status)")
            return out as? Data
        }

        /// Puts bytes in an item as they are, as something other than
        /// KrowkKeys might have.
        static func store(_ account: String, _ data: Data) throws {
            SecItemDelete(query(account) as CFDictionary)
            var add = query(account)
            add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
            add[kSecValueData as String] = data
            let status = SecItemAdd(add as CFDictionary, nil)
            try #require(status == errSecSuccess, "SecItemAdd \(account): OSStatus \(status)")
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
}
