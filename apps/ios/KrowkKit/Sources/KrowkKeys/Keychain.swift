import Foundation
import Security

/// The Keychain items krowk keeps on the iPhone (R-IOS-8): generic
/// passwords under one service, one per account name, each readable only
/// while the phone is unlocked, never synced to iCloud and never moved to
/// another device by a backup. No `kSecAttrAccessGroup` is set, so every
/// item is in the app's own default group (its application identifier),
/// which is the same however the app is signed: Xcode, or xtool with a
/// free Apple ID. A Notification Service Extension sharing them is M2.
enum Keychain {
    /// Every item krowk keeps: the slots and the wrapping key.
    static let service = "com.krowk.keys"

    /// One item: its class, service and account. Synchronizable false
    /// matches only items that are not synced (none ever is).
    static func query(_ account: String) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecAttrSynchronizable as String: false,
            kSecUseDataProtectionKeychain as String: true,
        ]
    }

    /// The item's bytes; nil when there is no such item.
    static func read(_ account: String) throws(KeychainFailure) -> Data? {
        var q = query(account)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        let status = SecItemCopyMatching(q as CFDictionary, &out)
        switch status {
        case errSecSuccess:
            guard let data = out as? Data else { throw KeychainFailure(op: "read", account: account, status: errSecDecode) }
            return data
        case errSecItemNotFound:
            return nil
        default:
            throw KeychainFailure(op: "read", account: account, status: status)
        }
    }

    /// Adds the item; with `replace`, an item already there has its bytes
    /// replaced (and its accessibility set again), else adding one that
    /// exists is `errSecDuplicateItem`. Either way the item is written
    /// whole: a reader sees the old bytes or the new.
    static func write(_ account: String, _ data: Data, replace: Bool) throws(KeychainFailure) {
        var add = query(account)
        add[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        add[kSecValueData as String] = data
        var status = SecItemAdd(add as CFDictionary, nil)
        if status == errSecDuplicateItem && replace {
            let update: [String: Any] = [
                kSecValueData as String: data,
                kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            ]
            status = SecItemUpdate(query(account) as CFDictionary, update as CFDictionary)
        }
        guard status == errSecSuccess else { throw KeychainFailure(op: "write", account: account, status: status) }
    }

    /// Removes the item; none there counts as removed.
    static func remove(_ account: String) throws(KeychainFailure) {
        let status = SecItemDelete(query(account) as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainFailure(op: "remove", account: account, status: status)
        }
    }

    /// Removes every item under the service, synced or not: what a first
    /// launch does with what an earlier install left behind.
    static func removeAll() throws(KeychainFailure) {
        let q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrSynchronizable as String: kSecAttrSynchronizableAny,
            kSecUseDataProtectionKeychain as String: true,
        ]
        let status = SecItemDelete(q as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw KeychainFailure(op: "remove every item of", account: service, status: status)
        }
    }
}

/// A Keychain call that failed: which, on what, and its OSStatus. Never the
/// item's bytes.
struct KeychainFailure: Error, CustomStringConvertible {
    let op: String
    let account: String
    let status: OSStatus

    var description: String {
        let reason = SecCopyErrorMessageString(status, nil).map { $0 as String } ?? "unknown error"
        let locked = status == errSecInteractionNotAllowed ? " (the iPhone is locked)" : ""
        return "could not \(op) the Keychain item \(account): OSStatus \(status), \(reason)\(locked)"
    }
}
