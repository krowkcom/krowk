import CryptoKit
import Foundation
import Krowk
import os
import Synchronization

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
/// Plaintext: Swift keeps none of its own. A slot's bytes reach `write` in
/// the `Data` UniFFI built, and `read` returns them in the `Data`
/// `AES.GCM.open` made, which UniFFI copies and frees; neither can be wiped
/// from here (Canon crypto.md → Zeroization). CryptoKit wipes the shared
/// secrets and AES keys it derives. Logs name a slot and an OSStatus, never
/// bytes.
///
/// Rust may call it from any thread; the Security framework is thread-safe,
/// and the stored properties are an immutable key handle and a flag behind
/// a lock. Ticket 78 exports no lock: the slot lock is Rust's, in-process
/// (krowk-ffi).
public final class KeychainKeyStorage: KeyStorage {
    /// The file whose presence says this install has already purged what an
    /// earlier one left in the Keychain: `Library/Application Support/
    /// krowk.installed`, empty, excluded from backups. Not `UserDefaults`,
    /// which a backup carries: restored onto a phone, it would say "already
    /// purged" over Keychain items sealed to a Secure Enclave key that the
    /// erase before the restore destroyed, and the app would be stuck on keys
    /// that never open. A file the backup leaves out makes any restore a
    /// first launch. It holds no key.
    public static var installMarker: URL {
        URL.applicationSupportDirectory.appending(path: "krowk.installed", directoryHint: .notDirectory)
    }

    /// The `KrowkError.Refused` code of a device with no Secure Enclave.
    public static let noSecureEnclave = "no_secure_enclave"

    private static let ephemeralLength = 65
    /// AES-GCM's nonce and tag.
    private static let boxOverhead = 12 + 16

    /// The service the app keeps its keys under.
    public static let appService = Keychain.appService

    private let wrappingKey: any WrappingKey
    private let items: Keychain
    /// Set by `forgetAll`: every call after it is refused.
    private let forgotten = Mutex(false)

    /// Opens the install's keys, making its wrapping key on first use. On
    /// the first launch after an install it first deletes every item an
    /// earlier install left (Keychain items outlive an app's deletion), so
    /// a reinstall never inherits a device identity. On a device with no
    /// Secure Enclave it throws `Refused` with the code `no_secure_enclave`:
    /// krowk keeps no keys there. Anything else (the phone locked, a
    /// Keychain error) is `Internal`, and may pass.
    ///
    /// `service` and `installMarker` are for tests, which keep a fixture's
    /// keys under a service of their own; the app keeps the defaults.
    public init(
        service: String = KeychainKeyStorage.appService, installMarker: URL = KeychainKeyStorage.installMarker
    ) throws(KrowkError) {
        let keychain = Keychain(service: service)
        items = keychain
        wrappingKey = try opening.withLock { _ throws(KrowkError) -> any WrappingKey in
            try Self.purgeIfFirstLaunch(installMarker, keychain)
            do throws(WrappingKeyFailure) {
                return try WrappingKeys.loadOrCreate(keychain)
            } catch .noSecureEnclave {
                log.error("no Secure Enclave")
                throw .Refused(code: Self.noSecureEnclave, detail: WrappingKeyFailure.noSecureEnclave.description)
            } catch {
                log.error("no wrapping key: \(error.description, privacy: .public)")
                throw .Internal(detail: error.description)
            }
        }
    }

    /// Deletes every item under the service (`com.krowk.keys`) unless the
    /// marker says this install already did, then writes the marker. The purge comes first:
    /// a launch that dies between the two purges again, which loses
    /// nothing, since no key was made in between.
    private static func purgeIfFirstLaunch(_ marker: URL, _ keychain: Keychain) throws(KrowkError) {
        guard !FileManager.default.fileExists(atPath: marker.path(percentEncoded: false)) else { return }
        do throws(KeychainFailure) { try keychain.removeAll() } catch {
            log.error("first-launch purge failed: OSStatus \(error.status, privacy: .public)")
            throw .Internal(detail: error.description)
        }
        do {
            try FileManager.default.createDirectory(at: marker.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data().write(to: marker, options: .atomic)
            var values = URLResourceValues()
            values.isExcludedFromBackup = true
            var url = marker
            try url.setResourceValues(values)
        } catch {
            // A marker a backup would carry is worse than none: the next
            // launch purges again, which loses nothing.
            try? FileManager.default.removeItem(at: marker)
            log.error("could not write the install marker")
            throw .Internal(detail: "could not record that krowk's Keychain items were cleared: \(error.localizedDescription)")
        }
    }

    /// Forgets every key this storage keeps (ticket 117): every item under
    /// its service, the slots and the item holding the wrapping key — on a
    /// phone the Secure Enclave key's only handle, so it can never open
    /// anything again. The install marker stays: the next storage made is
    /// no first launch, and finds nothing. This storage refuses every call
    /// after it, so nothing still holding it writes a slot sealed to a key
    /// that is gone; the app makes a new one. Throws when the Keychain
    /// refused (the phone locked): nothing is refused then, and it may be
    /// called again.
    public func forgetAll() throws(KrowkError) {
        do throws(KeychainFailure) { try items.removeAll() } catch {
            log.error("forgetting every key failed: OSStatus \(error.status, privacy: .public)")
            throw .Internal(detail: error.description)
        }
        forgotten.withLock { $0 = true }
    }

    public func read(slot: String) throws(KrowkError) -> Data? {
        try Self.check(slot)
        try checkKept()
        guard let sealed = try keychain({ () throws(KeychainFailure) in try self.items.read(slot) }) else { return nil }
        return try open(sealed, slot: slot)
    }

    public func write(slot: String, bytes: Data) throws(KrowkError) {
        try Self.check(slot)
        try checkKept()
        let sealed = try seal(bytes, slot: slot)
        try keychain { () throws(KeychainFailure) in try self.items.write(slot, sealed, replace: true) }
    }

    public func remove(slot: String) throws(KrowkError) {
        try Self.check(slot)
        try checkKept()
        try keychain { () throws(KeychainFailure) in try self.items.remove(slot) }
    }

    // MARK: Sealing

    private func seal(_ plaintext: Data, slot: String) throws(KrowkError) -> Data {
        let ephemeral = P256.KeyAgreement.PrivateKey()
        let share = ephemeral.publicKey.x963Representation
        do {
            let key = derive(try ephemeral.sharedSecretFromKeyAgreement(with: wrappingKey.publicKey), share, slot)
            let box = try AES.GCM.seal(plaintext, using: key, authenticating: Data(slot.utf8))
            guard let combined = box.combined else { throw CryptoKitError.incorrectParameterSize }
            return Data([sealedFormat]) + share + combined
        } catch {
            log.error("sealing \(slot, privacy: .public) failed")
            throw .Internal(detail: "could not seal the Keychain item \(slot): \(error)")
        }
    }

    /// The slot's bytes, or an error: never the sealed bytes, and never
    /// "absent" for an item that is there but does not open.
    private func open(_ sealed: Data, slot: String) throws(KrowkError) -> Data {
        let unreadable = KrowkError.Internal(
            detail: "the Keychain item \(slot) does not open with this install's key: it was not sealed here, or not under this name"
        )
        guard sealed.count >= 1 + Self.ephemeralLength + Self.boxOverhead, sealed.first == sealedFormat else {
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

    /// Refused once `forgetAll` ran: the wrapping key this storage holds
    /// is gone from the Keychain.
    private func checkKept() throws(KrowkError) {
        if forgotten.withLock({ $0 }) {
            throw .Internal(detail: "this iPhone's keys were reset; krowk opens its key storage again")
        }
    }

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

/// Held while an install's keys are opened: the first-launch purge and the
/// wrapping key's purge-and-make, so two `KeychainKeyStorage`s made at once
/// in this process never purge each other's key.
private let opening = Mutex(())

/// The sealed format's version, its first byte.
private let sealedFormat: UInt8 = 1

/// Names the slot and the OSStatus, nothing else: never a slot's bytes.
private let log = Logger(subsystem: "com.krowk.app", category: "keychain")
