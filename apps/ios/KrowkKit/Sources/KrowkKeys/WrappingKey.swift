import CryptoKit
import Foundation
import Security

/// The P-256 key every slot is sealed to (R-IOS-8): one per install, kept
/// in the Keychain item `wrapping-key`. On an iPhone it lives in the Secure
/// Enclave and never leaves it; the item holds only the enclave's opaque
/// handle, which no other device's enclave can use. It does key agreement
/// and nothing else, and it is not krowk's protocol crypto: the slots it
/// seals are bytes krowk-client wrote, which Swift never reads.
protocol WrappingKey: Sendable {
    var publicKey: P256.KeyAgreement.PublicKey { get }
    func sharedSecretFromKeyAgreement(with publicKeyShare: P256.KeyAgreement.PublicKey) throws -> SharedSecret
}

/// The Keychain item the wrapping key is kept in, beside the slots.
let wrappingKeyAccount = "wrapping-key"

/// Why there is no wrapping key.
enum WrappingKeyFailure: Error, CustomStringConvertible {
    /// A device build on a phone with no Secure Enclave: there is no
    /// software fallback, so krowk keeps no keys on it.
    case noSecureEnclave
    case keychain(KeychainFailure)
    /// The item is there and did not open as a key on this device: left as
    /// it is, since replacing it would lose every slot sealed to it.
    case unusable(String)

    var description: String {
        switch self {
        case .noSecureEnclave:
            "this iPhone has no Secure Enclave, so krowk cannot keep its keys safely on it"
        case .keychain(let f):
            f.description
        case .unusable(let why):
            "the Keychain item \(wrappingKeyAccount) did not open as a key on this iPhone: \(why)"
        }
    }
}

extension SecureEnclave.P256.KeyAgreement.PrivateKey: WrappingKey {}

#if targetEnvironment(simulator)
/// The simulator has no Secure Enclave. A simulator build seals the slots
/// to a software key kept in the same Keychain item (its raw 32 bytes), so
/// the tests cover everything but the enclave. A device build has no such
/// fallback: none of this is compiled into it.
extension P256.KeyAgreement.PrivateKey: WrappingKey {}
#endif

enum WrappingKeys {
    /// The install's wrapping key: read from its Keychain item, or made and
    /// stored there when there is none. With none, any slot left under the
    /// service was sealed to a key that is gone and can never open, so the
    /// service is cleared first rather than leaving reads that fail for
    /// ever. The caller holds `opening`, so no other opener in this process
    /// is between its purge and its key; two processes (none in M1) racing
    /// to store one agree on whichever was stored first.
    static func loadOrCreate(_ keychain: Keychain) throws(WrappingKeyFailure) -> any WrappingKey {
        if let key = try load(keychain) { return key }
        do throws(KeychainFailure) {
            try keychain.checkReadable()
            try keychain.removeAll()
        } catch { throw .keychain(error) }
        let made = try make()
        do throws(KeychainFailure) {
            try keychain.write(wrappingKeyAccount, made.stored, replace: false)
            return made.key
        } catch where error.status == errSecDuplicateItem {
            // Another caller stored one first: use that one.
            if let key = try load(keychain) { return key }
            throw .keychain(error)
        } catch {
            throw .keychain(error)
        }
    }

    private static func load(_ keychain: Keychain) throws(WrappingKeyFailure) -> (any WrappingKey)? {
        let stored: Data?
        do throws(KeychainFailure) { stored = try keychain.read(wrappingKeyAccount) } catch { throw .keychain(error) }
        guard let stored else { return nil }
        return try restore(stored)
    }

    #if targetEnvironment(simulator)
    private static func make() throws(WrappingKeyFailure) -> (key: any WrappingKey, stored: Data) {
        let key = P256.KeyAgreement.PrivateKey()
        return (key, key.rawRepresentation)
    }

    private static func restore(_ stored: Data) throws(WrappingKeyFailure) -> any WrappingKey {
        do { return try P256.KeyAgreement.PrivateKey(rawRepresentation: stored) } catch {
            throw .unusable("\(error)")
        }
    }
    #else
    /// A key made in the Secure Enclave, non-exportable, usable whenever
    /// the phone is unlocked and only on this device. No biometry: every
    /// registry call and the relay join read keys, and a Face ID prompt on
    /// each would make the app unusable. Face ID guards approvals (ticket
    /// 91), not key reads.
    private static func make() throws(WrappingKeyFailure) -> (key: any WrappingKey, stored: Data) {
        guard SecureEnclave.isAvailable else { throw .noSecureEnclave }
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(
            nil, kSecAttrAccessibleWhenUnlockedThisDeviceOnly, .privateKeyUsage, &error
        ) else {
            let why = error.map { "\($0.takeRetainedValue())" } ?? "no reason given"
            throw .unusable("could not make its access control: \(why)")
        }
        do {
            let key = try SecureEnclave.P256.KeyAgreement.PrivateKey(accessControl: access)
            return (key, key.dataRepresentation)
        } catch {
            throw .unusable("the Secure Enclave did not make a key: \(error)")
        }
    }

    private static func restore(_ stored: Data) throws(WrappingKeyFailure) -> any WrappingKey {
        guard SecureEnclave.isAvailable else { throw .noSecureEnclave }
        do { return try SecureEnclave.P256.KeyAgreement.PrivateKey(dataRepresentation: stored) } catch {
            throw .unusable("\(error)")
        }
    }
    #endif
}
