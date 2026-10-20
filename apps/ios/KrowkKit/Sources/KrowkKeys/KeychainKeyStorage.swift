import CryptoKit
import Foundation
import Krowk
import os

/// Where the iPhone keeps krowk's keys (R-IOS-8, R-FFI-3): every slot
/// krowk-client's keystore writes, and the krowk API key, as a Keychain
/// item sealed to the install's Secure Enclave key. Nothing goes to a file.
///
/// The app makes one at launch and hands it to the bindings' constructors
/// that take a `KeyStorage` (`Account.open`, `SignIn.wait`). Rust then
/// reads and writes the slots by name (`device.json`, `signing.json`,
/// `credentials.json`, …). This class only moves bytes: it never parses,
/// derives or uses a krowk key, and every rule about what may be written
/// stays in Rust (R-CLIENT-1, R-E2E-2). The sealing below is platform key
/// storage, like the Keychain's own encryption, not krowk's protocol crypto.
///
/// A slot is stored as `1 ‖ ephemeral P-256 public key (x963, 65 bytes) ‖
/// AES-GCM combined box`, where the AES key is HKDF-SHA256 over the ECDH of
/// the ephemeral key and the wrapping key (salt: the ephemeral public key;
/// info: "krowk ios keychain v1 " + slot), and the slot's name is the
/// authenticated data: an item copied to another name does not open.
///
/// Rust may call it from any thread; the Security framework is thread-safe
/// and the only stored property is an immutable key handle. ticket 78
/// exports no lock: the slot lock is Rust's, in-process (krowk-ffi).
public final class KeychainKeyStorage: KeyStorage {
    /// The `UserDefaults` key set once a launch has purged what an earlier
    /// install left in the Keychain.
    public static let installedMarker = "krowk.installed"

    static let format: UInt8 = 1
    private static let ephemeralLength = 65
    /// AES-GCM's nonce and tag.
    private static let boxOverhead = 12 + 16

    private let wrappingKey: any WrappingKey

    /// Opens the install's keys, making its wrapping key on first use. On
    /// the first launch after an install it first deletes every item an
    /// earlier install left (Keychain items outlive an app's deletion), so
    /// a reinstall never inherits a device identity. Throws on a device
    /// with no Secure Enclave: krowk keeps no keys there.
    public convenience init() throws(KrowkError) {
        try self.init(defaults: .standard)
    }

    init(defaults: UserDefaults) throws(KrowkError) {
        try Self.purgeIfFirstLaunch(defaults)
        do {
            wrappingKey = try WrappingKeys.loadOrCreate()
        } catch {
            log.error("no wrapping key: \(error.description, privacy: .public)")
            throw .Internal(detail: error.description)
        }
    }

    /// Deletes every item under `com.krowk.keys` unless `defaults` says this
    /// install already did, then says so.
    static func purgeIfFirstLaunch(_ defaults: UserDefaults) throws(KrowkError) {
        guard !defaults.bool(forKey: installedMarker) else { return }
        do throws(KeychainFailure) { try Keychain.removeAll() } catch {
            log.error("first-launch purge failed: OSStatus \(error.status, privacy: .public)")
            throw .Internal(detail: error.description)
        }
        defaults.set(true, forKey: installedMarker)
    }

    public func read(slot: String) throws(KrowkError) -> Data? {
        try Self.check(slot)
        guard let sealed = try keychain({ () throws(KeychainFailure) in try Keychain.read(slot) }) else { return nil }
        return try open(sealed, slot: slot)
    }

    public func write(slot: String, bytes: Data) throws(KrowkError) {
        try Self.check(slot)
        let sealed = try seal(bytes, slot: slot)
        try keychain { () throws(KeychainFailure) in try Keychain.write(slot, sealed, replace: true) }
    }

    public func remove(slot: String) throws(KrowkError) {
        try Self.check(slot)
        try keychain { () throws(KeychainFailure) in try Keychain.remove(slot) }
    }

    // MARK: Sealing

    func seal(_ plaintext: Data, slot: String) throws(KrowkError) -> Data {
        let ephemeral = P256.KeyAgreement.PrivateKey()
        let share = ephemeral.publicKey.x963Representation
        do {
            let key = derive(try ephemeral.sharedSecretFromKeyAgreement(with: wrappingKey.publicKey), share, slot)
            let box = try AES.GCM.seal(plaintext, using: key, authenticating: Data(slot.utf8))
            guard let combined = box.combined else { throw CryptoKitError.incorrectParameterSize }
            return Data([Self.format]) + share + combined
        } catch {
            log.error("sealing \(slot, privacy: .public) failed")
            throw .Internal(detail: "could not seal the Keychain item \(slot): \(error)")
        }
    }

    /// The slot's bytes, or an error: never the sealed bytes, and never
    /// "absent" for an item that is there but does not open.
    func open(_ sealed: Data, slot: String) throws(KrowkError) -> Data {
        let unreadable = KrowkError.Internal(
            detail: "the Keychain item \(slot) does not open with this install's key: it was not sealed here, or not under this name"
        )
        guard sealed.count >= 1 + Self.ephemeralLength + Self.boxOverhead, sealed.first == Self.format else {
            log.error("\(slot, privacy: .public) is not in krowk's sealed format")
            throw unreadable
        }
        let shareStart = sealed.startIndex + 1
        let boxStart = shareStart + Self.ephemeralLength
        do {
            let share = sealed[shareStart..<boxStart]
            let ephemeral = try P256.KeyAgreement.PublicKey(x963Representation: share)
            let key = derive(try wrappingKey.sharedSecretFromKeyAgreement(with: ephemeral), Data(share), slot)
            return try AES.GCM.open(AES.GCM.SealedBox(combined: sealed[boxStart...]), using: key, authenticating: Data(slot.utf8))
        } catch {
            log.error("opening \(slot, privacy: .public) failed")
            throw unreadable
        }
    }

    private func derive(_ secret: SharedSecret, _ share: Data, _ slot: String) -> SymmetricKey {
        secret.hkdfDerivedSymmetricKey(
            using: SHA256.self, salt: share, sharedInfo: Data("krowk ios keychain v1 \(slot)".utf8), outputByteCount: 32
        )
    }

    // MARK: Names and failures

    /// A slot name Rust may use: not empty, and not the wrapping key's
    /// item, which no slot may read, replace or remove.
    private static func check(_ slot: String) throws(KrowkError) {
        guard !slot.isEmpty, slot != wrappingKeyAccount else {
            throw .InvalidInput(detail: "\"\(slot)\" is not a name krowk keeps a key under")
        }
    }

    /// A Keychain call, its failure logged by slot and OSStatus and turned
    /// into the error Rust sees.
    private func keychain<T>(_ call: () throws(KeychainFailure) -> T) throws(KrowkError) -> T {
        do throws(KeychainFailure) { return try call() } catch {
            log.error("keychain \(error.op, privacy: .public) \(error.account, privacy: .public): OSStatus \(error.status, privacy: .public)")
            throw .Internal(detail: error.description)
        }
    }
}

/// Names the slot and the OSStatus, nothing else: never a slot's bytes.
private let log = Logger(subsystem: "com.krowk.app", category: "keychain")
