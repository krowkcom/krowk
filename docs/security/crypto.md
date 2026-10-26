# End-to-end encryption

> Published from Canon, krowk's private design repository (`engineering/crypto.md` at 9c0ec67). `threat-model.md` is in this directory. The other design files it cites, such as `relay.md`, `harness.md` and `devices.md`, are not published.

Sessions leave the machine they ran on — to the registry, the relay and R2 — so that a person can pick one up on another device. A transcript is the most sensitive thing krowk touches: source code, credentials pasted into a prompt, whatever a tool read. This file is the crypto design every sync, relay and client ticket reads instead of choosing again: the constructions and their suite, the key hierarchy, the blob formats, what the server side can and cannot learn, and what is not done yet.

The design assumes the server side is hostile (spec R-OSS-1). The registry, the relay and R2 are krowk's own, and they are still treated as an attacker who holds every byte they have ever been sent, can change or replay any of it, and can answer any request with anything. Nothing in this file relies on the server behaving well for confidentiality or integrity; it can only deny service.

The code is `crates/krowk-client` in `krowkcom/krowk`, the one crate every client links (R-CLIENT-1): the terminal today, the desktop app (gpui) directly and the phones through UniFFI later. Crypto is implemented there and nowhere else (R-E2E-2); a test (`tests/crypto_boundary.rs`) fails when any other crate in the workspace names a crypto crate as a dependency. Built with Harness P1 ticket 15, which covers the local keys and wrapping; the relay's use of the frame encryption is 17 and 19, and the threat model this design answers to is `threat-model.md` (Harness P2 ticket 28).

**Devices you own.** Keys belong to a person, not a workspace (`devices.md`, and the decision record `decisions/2026-09-30-devices-belong-to-people.md`). The client's side of that is built in `krowk-client`: the user key and its generations (`user_key`), the recovery kit and the device it derives (`recovery`), the signed device list and its verifier (`device_chain`), and pairing by a short code (`pairing`). The CLI commands on top of them and the registry's side shipped with krowk v0.12.0 (2026-10-04). Ticket 16's flow — an account key per workspace, a 24-word phrase and `krowk devices approve` by the device's id — is gone from the terminal, and the registry answers its routes `410 sync_reset` (`devices.md` → Registry plumbing → Clean break). This file states the new design, not that flow.

## Constructions

Only established constructions, from audited crates (R-E2E-2), and one suite of each — no negotiation, so there is no downgrade to negotiate.

| use | construction | crate |
|---|---|---|
| wrap the user key to a device | HPKE (RFC 9180), Base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305 — suite `0x0020 / 0x0001 / 0x0003` | `hpke` 0.14 (rozbb) |
| wrap user key generation g under g+1; a session key under the user key | XChaCha20-Poly1305 | `chacha20poly1305` 0.11 (RustCrypto) |
| seal session content (frames, chunks) | XChaCha20-Poly1305 under the session key | `chacha20poly1305` 0.11 |
| key and device ids, the device list's hash chain, the kit's checksum | SHA-256 | `sha2` 0.11 (RustCrypto) |
| derive the recovery device from the kit; pairing's keys | HKDF-SHA256 (RFC 5869) | `hkdf` 0.13 (RustCrypto) |
| pairing's key confirmation | HMAC-SHA256 | `hmac` 0.13 (RustCrypto) |
| pair a device by a short code | SPAKE2, Ed25519 group, asymmetric — magic-wormhole's construction, not RFC 9382's | `spake2` =0.5.0-pre.0 (RustCrypto) |
| device-list entries, relay joins, registry requests | Ed25519 (RFC 8032), verified strictly | `ed25519-dalek` 3 (dalek), no precomputed tables |
| randomness | the operating system's generator (`getrandom`) | `getrandom` 0.4 |

**Why these crates, and not ring.** ring is already linked, for TLS, and it has X25519 and ChaCha20-Poly1305 — but neither XChaCha20-Poly1305 nor HPKE, and R-E2E-2 names both. Building HPKE on ring's primitives would be writing the composition ourselves, which is the thing R-E2E-2 forbids. The `hpke` crate is a pure-Rust composition over the dalek and RustCrypto primitives, and chosen over Cryspen's `hpke-rs`: `hpke`'s dependencies are the primitive crates krowk would pick anyway, at the versions it links already (`sha2`, `getrandom`), where `hpke-rs` links `libcrux-sha3` and `log` for every suite and has since moved into libcrux. With the phrase's word list and `krowk sync`, the whole of it adds 93,760 bytes (about 92 KiB) to the full build, inside its budget (`full.size` in `crates/krowk-bench/budgets.toml`); the agent build links none of it.

**Audit status, stated exactly.** curve25519-dalek, under x25519-dalek, was audited by Quarkslab ([report](https://blog.quarkslab.com/security-audit-of-dalek-libraries.html), an earlier version; the crate's own README does not mention it). RustCrypto's `chacha20poly1305` was reviewed by NCC Group in 2020 (an earlier version), with no significant findings. The `hpke` crate has had no paid audit; Cloudflare reviewed its version 0.8 internally and found no issues. What krowk adds is a check, not an audit: RFC 9180's own test vector for the pinned suite passes in both directions — the recipient's key derivation, the sender's encapsulation and every ciphertext reproduced byte for byte, opening, and the exporter (`tests/rfc9180.rs`). **The `spake2` crate has had no audit of any kind.** It is pinned to its one prerelease, `=0.5.0-pre.0`, which runs 0.4's algorithm on the curve25519-dalek 5, `sha2` 0.11 and `hkdf` 0.13 krowk already links (0.4 would link a second dalek and `sha2` 0.10 beside them). Its M, N and S points are python-spake2's, and what krowk adds is again a check: python-spake2's asymmetric vector — the one magic-wormhole's implementation produces — reproduced through the crate's public API, both messages and the shared key byte for byte (`tests/pairing.rs`, `d2_spake2_matches_the_magic_wormhole_vectors`), so a change in the crate fails the build. `hkdf` and `hmac` are linked already, by `hpke`. An independent audit of this design and its code is spec R-OSS-2, before Pro sync reaches general availability; its scope is `threat-model.md` → The independent audit, and it is Harness P2 ticket 37.

## The key hierarchy

```
recovery kit (12 words, 128 bits)  ─ HKDF ─►  recovery device: X25519 + Ed25519, on the device list
                                                   │
user key  K_user, generation g  ◄── HPKE-wrapped to every device on the verified list, the recovery device included
        │   (K_user g+1 wraps K_user g, so the newest generation opens every older one)
        │
        │  wraps, XChaCha20-Poly1305
        ▼
session key  K_sess  (32 random bytes, one per session)
        │
        │  seals, XChaCha20-Poly1305
        ▼
session content: batches, frames, chunks
```

Why each layer is there, and what a person does with it, is `devices.md` → Keys. What each is, exactly:

- **Device key.** An X25519 keypair per machine and krowk home: RFC 9180's `DeriveKeyPair` over 32 bytes from the operating system. The private half never leaves the machine. A new krowk home is a new device. A device's id is the first 16 bytes of `SHA-256("krowk/device-id/v1" ‖ X25519 public key)`.
- **Signing key.** An Ed25519 keypair per device (`e2e::SigningKey`), in `signing.json` beside `device.json`. It signs three things, each under its own label: a relay join (`krowk/relay/v1`, `relay.md` → Joining), a registry request (`krowk/registry/v1`, below) and a device-list entry (`krowk/device-list/v1`, below). No key is wrapped to it.
- **User key.** 32 random bytes per person, not per workspace, with a generation that starts at 1 and goes up by one on every rotation (a removal, a kit replacement). Its id is the first 16 bytes of `SHA-256("krowk/user-key-id/v1" ‖ generation (u32, big-endian) ‖ key)`, so two generations never share an id. It is held only wrapped: each generation to devices (HPKE), and each generation g under g+1 (XChaCha20-Poly1305), so a device holding the newest opens any older one by walking down one step at a time (`UserKeys::open`). It is not the kit, and nothing but a device holds it.
- **Adopting a generation.** HPKE Base mode doesn't say who wrapped a key: the registry, holding a device's public key, can wrap a key of its own to it. So a device takes generation g only when the opened key's computed id is the id its verified device list names for g — never the id in the blob's header, which is the sender's claim — and, when it already holds an older generation, only when the new one opens back down to exactly the one it holds (`UserKeys::adopt`).
- **Recovery device.** The kit's 16 bytes through HKDF-SHA256 with no salt and `info` `"krowk/recovery-device/v1"`, expanded to 64 bytes: the first 32 are `DeriveKeyPair`'s input for the X25519 key, the last 32 the Ed25519 seed. The same words give the same device on every machine; it sits on the device list with kind `recovery` and is wrapped to like any other device, so a rotation never makes the kit stale. Known answers for two kits are frozen in `recovery.rs`'s tests: a change to the label or the split fails them, since it would orphan every kit written.
- **Session key.** 32 random bytes per session, made by the host that owns it, wrapped under the owner's user key at the generation current when it is sealed, and stored beside the session's chunks. Everything the session writes is sealed under it. Sharing one session — a share link, a hosted runner (R-E2E-5), later a teammate — wraps that session's key for them and nothing else.
- **Vintage key.** 32 random bytes per vintage, made each time a vintage is sealed (a merge seals the union under a new one) and carried, wrapped, in the vintage's own blob (below), so the registry stores nothing beside the bytes. It seals only that week's archive. It is wrapped under the user key at the generation the verified device list names current when it is sealed, as a session key is, and opened by walking down the user keys to the generation the blob names, so a vintage sealed before a rotation opens after it, and a device removed since holds no generation sealed after its removal (Harness P2 ticket 41).

On disk (in krowk's home, `0600`, replaced by rename through the credentials file's atomic write): `device.json` — `{"version": 1, "secret": "<hex X25519 private key>"}` — and `signing.json`. The user key is stored on a device only wrapped to it, as the account key was before it; the file's shape is the CLI's, being built. The registry holds the wraps — per person, generation and device, and each generation under the next — and never the user key in any form it can open.

**krowk-client's keystore keeps every key-storage rule — the pin check, generation monotonicity, the device-id cross-check — and reaches storage only through the KeyStorage seam. Each platform supplies the storage (files in krowk's home on desktop, the Keychain on iOS) and never re-implements a rule** (R-CLIENT-4, proposed with Harness P4 ticket 63). `keystore::Keystore` decides what may be written; `KeyStorage` reads, replaces atomically, removes and locks one `Slot` at a time — the device key, the signing key, the account key, the user keys, the device list — and knows nothing of what the bytes mean. `FileStorage` is the files above, byte for byte, and its locks the `.lock` files beside them (`account-key.lock`, `device-list.lock`); `MemoryStorage` is for tests and fakes, and the policy tests run on both. A platform's store is an implementation of the trait, not a second keystore, so a rule changed in Rust holds on every platform at once (R-CLIENT-1).

**On the iPhone every key slot krowk keeps is a Keychain item, accessible only when the device is unlocked and never leaving this device (`kSecAttrAccessibleWhenUnlockedThisDeviceOnly`, not synchronizable), sealed under a non-exportable Secure Enclave P-256 key; no key is written to a file in the app container** (spec R-IOS-8, proposed with Harness P4 ticket 82). The device key is X25519 (R-E2E-2, R-E2E-3), which the Secure Enclave cannot hold — it does P-256 only — so the enclave wraps it instead, and the signing key, the user keys, the pinned device list and the krowk API key (`credentials.json`) with it: any extraction needs this phone and an unlock. The app's `KeychainKeyStorage` (`apps/ios/KrowkKit/Sources/KrowkKeys`) is the `KeyStorage` of R-FFI-3. Each slot is a generic-password item, service `com.krowk.keys`, account the slot's name (`device.json`, …), with no access group set, so it is in the app's own default group. One Secure Enclave key per install does key agreement only. Its access control is `.privateKeyUsage` with no biometry: every registry call and the relay join read keys, and Face ID guards approvals, not reads. It is kept, as the enclave's opaque handle that only this device's enclave can use, in the item `wrapping-key`, with the same attributes. A slot is sealed by ECDH between a fresh ephemeral P-256 key and the enclave key, HKDF-SHA256 (salt the ephemeral public key's X9.63 bytes, info `"krowk ios keychain v1 " ‖ slot name`) to 32 bytes, and AES-GCM with the slot's name as authenticated data, stored as `1 ‖ ephemeral public key (65 bytes) ‖ nonce ‖ ciphertext ‖ tag`. An item that is there and does not open — copied under another name, sealed by another install, damaged — is an error, never "absent" and never its stored bytes. Keychain items outlive the app, so the first launch after an install deletes every `com.krowk.keys` item before anything reads one, then writes an empty marker file, `Library/Application Support/krowk.installed`, excluded from backups: a reinstall starts as a new device instead of inheriting an identity whose wrapping key may be gone. The marker is a file a backup leaves out rather than a `UserDefaults` key, which a backup carries: a phone erased and restored gets its `ThisDeviceOnly` items back, sealed to an enclave key the erase may have destroyed, and must purge them rather than risk failing on them for ever. With no wrapping key at all — its item not found, and another item under the service read successfully, so a locked or failing Keychain is never taken for an empty one — any slot left is cleared before a new key is made, since nothing could open it. The simulator has no Secure Enclave, so under `#if targetEnvironment(simulator)` alone the wrapping key is a software P-256 key in the same item; a device build has no fallback, and with no Secure Enclave the storage refuses to open (`Refused`, code `no_secure_enclave`) and the app says it cannot keep keys safely; any other failure, such as a locked phone, says the keys could not be opened and is retried when the app comes forward. This is platform key storage, like the Keychain's own encryption, and not a second implementation of krowk's crypto (R-E2E-2, R-CLIENT-1): CryptoKit seals opaque bytes at rest, and Swift never parses, derives or uses a krowk key. HPKE, XChaCha20-Poly1305, Ed25519 and SPAKE2 stay in krowk-client, and so does every rule about what may be written (R-CLIENT-4).

## The device list

A key rotation is only as good as the list it rotates to, and the registry can't be trusted to say which devices a person has. So the list is an append-only chain of signed entries that every client verifies itself (`device_chain`). The flow and the reasons are `devices.md` → The device list and → Replacing the kit; the encoding and the rules are these.

**An entry's bytes**, big-endian throughout, every field fixed or length-prefixed:

| field | size | value |
|---|---|---|
| version | 1 | 1 |
| seq | 8 | 0 for the first entry, one more for each |
| prev | 32 | SHA-256 of the previous entry's bytes; zeros at seq 0 |
| action | 1 | 1 `add`, 2 `remove`, 3 `rotate-recovery` |
| subjects | 1 | how many: 1, or 2 at seq 0 (the first device, then the recovery device) |
| — kind | 1 | 1 `device`, 2 `recovery` |
| — name | 2 + n | UTF-8, 1–64 bytes |
| — os | 2 + n | UTF-8, 0–32 bytes |
| — x25519 | 32 | the key the user key is wrapped to |
| — ed25519 | 32 | the key it signs with |
| generation | 4 | the user key generation the entry leaves current |
| key id | 16 | that generation's user key id |
| carried | 1 or 17 | 0; or, at seq 0 of a chain started over, 1 then the previous chain's newest user key id (16) |
| time | 8 | Unix seconds, by the signer's clock; informational, nothing checks it |
| signers | 1 + 16 each | 1–3 device ids, strictly ascending |

A reader decodes strictly — an unknown version, action or kind, bad UTF-8, a length or count out of range, unsorted signers, trailing bytes — and then re-encodes and refuses anything that doesn't come back byte for byte, so an entry has exactly one encoding. Names and OS strings refuse control characters, Unicode's format characters (the direction marks and overrides, the zero widths) and the line and paragraph separators: a name is what a person reads at every prompt and in recovery's review. A name may not repeat, ignoring case, one a device on the list already has.

**The signatures** travel beside the bytes, not in them: `count (1)`, then for each signer in the order the bytes list them, `device id (16) ‖ Ed25519 signature (64)` over `"krowk/device-list/v1" ‖ bytes`, verified strictly. The ids must be exactly the bytes' `signers`, so the hash that chains and pins an entry — SHA-256 over the bytes alone — also commits to who signed it. Who must sign:

- **Every device an entry adds, as proof of possession**: both subjects at seq 0, and the new recovery device in `rotate-recovery`. Without this a registry holding only the kit's public keys could build a whole chain around the person's real kit, and a recovering machine would accept it.
- **Except at seq 0, exactly one authorizer**: a device on the list and not removed before this entry. Any device authorizes every action but `rotate-recovery`, which only the current recovery device does — or any device, when the list has none.

**What the verifier holds each entry to:**

- `seq` is one more than the previous entry's, and `prev` is its hash.
- Seq 0 adds a `device`, and may add one `recovery` device as its second subject; it leaves generation 1.
- `add` adds a `device` whose keys the chain has never held — X25519 and Ed25519 are one pool, a removed device's included — and leaves the generation and key id as they were.
- `remove` names an active `device` exactly as the list holds it, every field. A `recovery` device can't be removed; it is only replaced.
- `rotate-recovery` replaces the recovery device at once, or makes the first when setup skipped the kit.
- A `remove` and a `rotate-recovery` rotate the key: generation exactly one higher, with a key id the chain has never held. An `add` carries the current key id unchanged.
- `carried` is 0 at every seq but 0. It says nothing to the chain's own rules: it names the key the new generation 1 wraps as its link back (Starting over, below).

**Pinning.** A device that has verified the list before holds its head, `seq` and hash (`Chain::verify` takes it). It refuses a list shorter than its pin, or with another hash at the pin's `seq`: an older list, or a forked one. Any entry refused refuses the whole list. A client wraps to, adopts keys from and pins only a list that verified.

**A batch.** N removals are N entries and N generations, g+1 … g+N (`Chain::batch`), posted whole. Only g+N is wrapped to devices — every device left on the list, and the recovery device — and each of g … g+N−1 is wrapped under the generation after it. The generations between are wrapped to no device, so a device removed later in the batch never holds a generation after its own removal.

**The registry runs the same rules.** An entry it stores stays for ever, so one the clients refuse would freeze every device's list, and with it the removal of a stolen laptop. The two verifiers are held equal by a corpus of accept and refuse cases with reason codes, deterministic keys and signatures, which `krowk-client` writes (`tests/device_chain_corpus.rs`, `tests/fixtures/device_chain_corpus.json`; it fails when the file would change) and the registry's suite replays.

**Starting over.** A new chain for a person who has one (`devices.md` → Starting over) is a new seq 0 under a new chain epoch, never a truncation. Started from a device that holds the user key, it carries the key forward: seq 0's `carried` names the previous chain's newest user key id, and the new generation 1 wraps that key in the link format below — the header naming the old key's own generation and id, the associated data binding it to generation 1 — whatever the old key's generation. A device opens it only as the id seq 0 carries (`UserKeys::carry`), and walks down the old chain's own links from there, so every session sealed before the start-over stays readable. A start-over with no device carries nothing.

## Blob formats

Every wrapped key is a versioned blob. Its first byte is the format version and its second the suite, and both are authenticated, so a format newer than the reader's is refused with "upgrade krowk" rather than misread; any other version or suite is refused like a changed byte. Lengths are fixed; a blob of any other length is refused before any crypto runs.

**A user key generation wrapped to a device** — 102 bytes:

| offset | size | field |
|---|---|---|
| 0 | 1 | version, 1 |
| 1 | 1 | suite, 1 = HPKE Base, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256, ChaCha20-Poly1305 |
| 2 | 4 | generation, big-endian |
| 6 | 16 | the generation's user key id |
| 22 | 32 | HPKE `enc` (the ephemeral public key) |
| 54 | 48 | the sealed user key and its 16-byte tag |

HPKE's `info` is `"krowk/user-key/v1" ‖ user key id (16) ‖ generation (4) ‖ device id (16)`, and its associated data the first 22 bytes. A blob moved to another device, or passed off as another generation, does not open, and the id the opener checks is the device list's, not the header's (above). Every wrap draws a fresh ephemeral key, so wrapping the same key twice gives two different blobs.

**Generation g wrapped under generation g+1** — 94 bytes:

| offset | size | field |
|---|---|---|
| 0 | 1 | version, 1 |
| 1 | 1 | suite, 2 = XChaCha20-Poly1305 |
| 2 | 4 | g, big-endian |
| 6 | 16 | g's user key id |
| 22 | 24 | nonce, random |
| 46 | 48 | the sealed key g and its tag |

Under key g+1, with associated data `"krowk/user-key-chain/v1" ‖ version ‖ suite ‖ id g ‖ g ‖ id g+1 ‖ g+1`: it opens only under the very next generation and only as the generation it names, and the opened key's id must be the header's. Two wraps naming one generation are refused, since a server offering both is shadowing one. Known answers for both blobs are frozen in `user_key.rs`'s tests.

**A session key wrapped under the user key** — 74 bytes, the size the registry holds a wrapped session key to:

| offset | size | field |
|---|---|---|
| 0 | 1 | version, 2 |
| 1 | 1 | suite, 2 = XChaCha20-Poly1305 |
| 2 | 4 | user key generation, big-endian |
| 6 | 20 | random |
| 26 | 48 | the sealed session key and its 16-byte tag |

The nonce is the 24 bytes from offset 2: the generation and 20 random bytes, 160 random bits a generation. The generation is in the clear so an opener knows how far down its `UserKeys` to walk, and a device holding only older generations can say so. The associated data is `"krowk/session-key/v2" ‖ version ‖ suite ‖ generation ‖ that generation's user key id ‖ session id (16 bytes, the session's UUID)`: moved to another session, or relabelled as another generation, it does not open. A device seals a new session only under the generation its verified list names as current (`e2e::seal_session_key`), so a device behind a rotation can't seal under a key a removed device holds. Version 1, ticket 15's wrap under the account key, is refused.

**A rotated session's key ring** (`e2e::wrap_session_keys`) — version 3, the same layout with every key epoch's session key sealed as one, oldest first: 42 + 32 × epochs bytes, from 2 to 64 epochs. The associated data is the version-2 one labelled `"krowk/session-keys/v3"`. A ring of one key is the version-2 blob above, byte for byte, so a session never rotated reads on an older krowk; a version-2 reader refuses a ring as newer.

**Key epochs.** A device that is removed held every key of every session it could open, so a session that goes on after the removal must stop being sealed under that key. When a host takes a session up (`sync::host::take`), holding its lease, and finds the wrapped key names an older user key generation than its verified list's, a device was removed since: before it writes anything, it adds a key epoch — a fresh session key, current from then on, the old ones kept in the ring — and writes the ring sealed under the current generation, a record signed over it, and the index sealed under the new key, in one request (an index left under an older epoch's key still opens, and the holder's next index write seals it under the current one). A start-over's re-seal adds a key epoch too, since a device of the old list that the new one does not name holds every key so far; it writes no index, which the fallback opens until the next host writes one. The registry takes that write from the lease holder only when the new key names a later generation and holds exactly one key more, and the record is new and names the writer (`SessionsController#rotatable?`). A rotation the registry refuses leaves the session unhosted. Relay frames and the index are sealed under the current key; chunks name theirs (below).

**What epochs don't close.** Until the session is taken up again, the removed device holds its current key, and so does a host that keeps running across the removal, which goes on under it. With the registry's help the removed device can also append chunks under the old key in that time, chained onto the log's tail and sealed as genuinely as the session's own: the host that rotates reads the log to the end the registry serves and chains onto it, and nothing a reader holds tells those chunks from real ones, since the index that names the head is sealed under the same key. What epochs give is that nothing it seals opens after the rotation's first chunk.

**A session's index**, sealed for the registry (`seal_session_index`) — the title and whatever else a listing shows, as one blob, the `sealed_index` the registry stores:

| offset | size | field |
|---|---|---|
| 0 | 1 | version, 1 |
| 1 | 1 | suite, 2 = XChaCha20-Poly1305 |
| 2 | 24 | nonce, random |
| 26 | n + 16 | the sealed index and its tag |

Under the session key, with associated data `"krowk/session-index/v1" ‖ version ‖ suite ‖ session id`: moved to another session, or presented as a wrapped key, it does not open. It is one message the holder replaces, so nothing orders it: a hostile registry can serve an older index than the latest, which shows a stale title and nothing else. What is inside it is the client's to define (ticket 19); the registry sees its length.

**A vintage** (`seal_vintage`) — one ISO week of archived sessions, zstd-compressed JSONL (`harness.md` → Vintages) — carries its own wrapped key:

| offset | size | field |
|---|---|---|
| 0 | 1 | version, 2 |
| 1 | 1 | suite, 2 = XChaCha20-Poly1305 |
| 2 | 4 | the user key generation it is sealed under, big-endian |
| 6 | 24 | key nonce, random |
| 30 | 48 | the vintage key sealed under that user key, and its tag |
| 78 | 24 | nonce, random |
| 102 | n + 16 | the sealed archive and its tag |

The key's associated data is `"krowk/vintage-key/v2" ‖ version ‖ suite ‖ generation ‖ user key id ‖ week`, the archive's `"krowk/vintage/v2" ‖ version ‖ suite ‖ generation ‖ user key id ‖ week`, the week as text (`2026-W38`). A vintage served for another week, under another person's user key, or with its generation changed, does not open; a vintage is sealed only after the device list is read from the registry and verified, and never under a generation older than the one it names current. Format 1 — the vintage key wrapped under the account key, before the user key — is refused by name: the account key went with the clean break, and no production registry ever stored one, since the vintage endpoints reached it only with this format; a format newer than the reader's is refused with "upgrade krowk". What the week does not stop: a registry serving an older vintage of the same week. That costs a session the reader then does not find in it, refused by name, never a session read wrong; the vintage's digest is checked before it is opened.

**A record sealed to the person's devices** (`seal_to_person`) is a vintage's layout with version 1, and `binding` where the week was. The key's associated data is `label ‖ "-key" ‖ version ‖ suite ‖ generation ‖ user key id ‖ binding`, the body's `label ‖ version ‖ suite ‖ generation ‖ user key id ‖ binding`. The label is one of a fixed set (`e2e::Label`), so a record of one kind never opens as another. It is sealed under the generation the verified device list names current, never an older one, so a device removed since does not open it. A host's announcement (`presence`, R-HOST-3) is the first: label `"krowk/presence/v1"`, bound to the 16-byte id of the device it is about, its JSON padded with spaces to a multiple of 1,024 bytes and at most 12 KiB. The opener refuses one from a device not on its verified list or from the recovery kit, and one sealed under a generation older than its list's current one: the construction is symmetric, and a device removed since still holds the older generations, so without that floor it could write a record as any device still listed. It shows the list's name and OS for the device, not the record's. What the binding does not stop: a registry serving an older announcement from the same device, since nothing binds freshness, and a device still on the list writing a record about another, since the binding is not a signature.

**A command one device sends a host, and its answer** (`host_command`, R-HOST-4) are records of the same kind: labels `"krowk/host-command/v1"` and `"krowk/host-answer/v1"`, each bound to `command id (lower case) ‖ from device id ‖ to device id`, so one moved to another id, or to another pair of devices, does not open, and an answer never opens as a command. The host opens a command only from a device on its verified list, not the recovery kit, sealed under the list's current generation; one that does not open gets no answer, since an answer would need a key the sender proved it holds. As with an announcement, the binding is not a signature: a device still on the list could seal a command naming another listed device as its sender, which only the registry's signed request ties to the device that sent it — though it could send the same command as itself. A command is a `start` (a directory, a model and a prompt) or a `take` (a session id and a directory, R-HAND-1, R-HAND-4); a type the host does not know is refused by name, and any field a command carries beyond its own is ignored: the permission mode is the host's own. Nothing binds freshness here either: a registry can deliver a command again, which the host answers from the last 4,096 ids it kept, and ids that did not open are kept apart, so they never push an answered one out. A registry that delivers a command again after the host restarts has it run again: nothing sealed says when it was sent, and the ten minutes a command is live are the registry's word. It runs only what the person once asked, in a directory the host offers, in the host's own mode; a `take` delivered again moves the session back here if it has left since, is answered `taken` if it has not, and is refused `busy` while the first take still runs.

**Session content.** A sealed payload is `counter (8, big-endian) ‖ nonce (24, random) ‖ ciphertext ‖ tag (16)`, under the session key. The header's `enc` byte is **1** for this construction (`ENC_XCHACHA20_POLY1305` in `krowk_client::e2e`); 0 stays "none", a reader refuses a value it does not know, and sealing or opening refuses a header whose `enc` is not 1. The order is compress, then encrypt (ciphertext does not compress).

The associated data is `"krowk/frame/v1" ‖ header (28) ‖ session id (16) ‖ direction (1) ‖ epoch (16) ‖ counter (8)`. **The 28-byte header alone is not enough**, because it repeats: every batch with no line carries `seq` 0, and every client frame carries session 0 and `seq` 0, so under one key two messages would share associated data and either would open in the other's place — a recorded approval replayed, one control batch swapped for another. What makes every message's associated data unique under a session key:

- **direction** — 1 host → client (batches), 2 client → host (frames). Each direction is its own chain, so a message cannot be reflected back at its sender.
- **counter** — per session, per direction, per epoch: 0 for the first message, one more for each. The receiver (`Opener`) refuses a counter that is not strictly greater than the last one it opened, so a replay or a reordering does not open. A gap opens — the transport may legitimately drop batches and catch a client up from its cursor — and the opener says where the chain now is.
- **epoch** — 16 random bytes the *receiver* of a direction picks for each connection (`Opener::new`) and sends in the clear when it opens; the sender binds it (`Sealer::new`). A recording of one connection opens on no later one, because the receiver has picked a fresh epoch.

The API enforces this: content is sealed only through `Sealer` and opened only through `Opener`, never with a bare key.

**What tickets 17 and 19 must do.** Seal every batch and every client frame that carries session content or a command about a session — `execute`, a prompt, `steer`, `interrupt`, an `approve` — under **that session's key**, through a `Sealer` for its direction; client frames name their session in the header, or in the sealed payload when the header's session is zero. Exchange each direction's epoch in the hello and welcome. Frames that carry no session content — an ack, and on the daemon's own socket its hello — are not sealed, and must not be trusted for anything a session key protects. Across the relay a viewer's hello *is* sealed under the session key, under the epoch derived from its link (below), because the relay refuses anything unsealed; it carries the viewer's challenge and nothing a session key protects. **The end of a stream is authenticated by a final sealed frame**: a sender that ends a session's stream on purpose seals a last message saying so, and a receiver treats a stream that stops without one — however cleanly the connection closed — as cut short, not finished. Resends after a reconnect are new messages under the new epoch, never the old ciphertexts. **Per-receiver epochs and the relay's ring buffer.** An epoch the receiver picks means ciphertext is bound to one receiver's connection, so the relay cannot hand buffered batches to a late or a second client (R-LAG-6's ring buffer, R-RELAY-1's fan-out): they would not open there. Ticket 17 must reconcile the two, and should take this option: for host → client the **host** picks a random epoch per stream, announces it to each receiver sealed under the session key with that receiver's own fresh challenge bound in, and each receiver accepts a given stream epoch once and then holds its counter strictly increasing — so buffered ciphertext of the current stream serves every client, while a recording of an earlier stream is refused because its epoch was already accepted or never announced. Client → host keeps the receiver-chosen epoch, since the host is the only receiver. If 17 chooses otherwise, it records why here. **Ticket 17 took this option** (`relay.md` → Epochs across the relay): the host's stream is named in its join, the relay keeps one ring buffer per stream, a viewer's challenge reaches the host alone and the host's announcement reaches that viewer alone, and what the host routes to one viewer is sealed in a chain of that viewer's own, since it is not ordered against the stream's batches. Sealing and opening them are ticket 19's. **Ticket 19 seals them** (`krowk_client::relay_link`). Batch `n` of a stream is sealed with counter `n − 1` under the stream's epoch and carries header `seq` `n`, and a viewer refuses a batch whose header and counter disagree. Each viewer's routed chain takes the viewer's **challenge** as its epoch — 16 random bytes it draws per connection and sends in its hello — so the host's first routed message, the welcome, which names the stream and the epoch the viewer's frames go under, is bound to that connection: a recorded welcome opens on no other. The one frame a viewer sends before the welcome, its hello, has no host-chosen epoch to go under, so it is sealed under one both ends derive from the viewer's link, `SHA-256("krowk/relay-hello/v1" ‖ link)`'s first 16 bytes, at counter 0, and it carries the challenge and nothing a session key protects: a hello replayed makes the host announce to a challenge nobody holds. A viewer accepts a stream once and keeps its counter across reconnects; a welcome that reuses the challenge as an epoch is refused, and a host opens a routed chain under a given challenge once, so a replayed hello opens no second chain under an epoch already used. A batch whose counter skips opens, as a gap should, and the viewer asks the host for what it missed (`harness.md` → Sync cursors). Every chain's plaintext starts with a flags byte whose bit 0 is the chain's final message.

Chunks at rest in R2 have no live receiver to pick an epoch. So a chunk binds its index in the log as the counter, and as the epoch a per-session value fixed when the session key is made: the first 16 bytes of `SHA-256("krowk/chunk-epoch/v1" ‖ session key)`, which every device holding the key derives alike and which needs no storing. **The log is a chain**: each chunk also binds the SHA-256 of the whole sealed chunk before it (32 zero bytes for chunk 0), and carries the lease fence it was written under. Without the chain, any holder of the session key can seal a valid chunk N, so a displaced holder's chunk 5 followed by the next holder's 6…, or either of two authentic chunk 5s, would open as one log — integrity resting on the registry. With it, a splice or a fork fails to open at the chunk after it. A reader (`ChunkReader`) opens index 0 first and each next one exactly one more, each bound to the one it read before, with a fence never lower than the last one's, and nothing after the final chunk. A holder taking up a log reads it to its end first and chains onto that (`ChunkSealer::new` takes the next index, the previous chunk's digest and its own fence). Built with ticket 16b (`ChunkSealer`, `ChunkReader` in `krowk_client::e2e`); ticket 19 writes them from the host.

**A sealed chunk**: `version (1) = 1 ‖ suite (1) = 2 ‖ flags (1; bit 0 final) ‖ index (8, big-endian) ‖ fence (8, big-endian) ‖ nonce (24, random) ‖ ciphertext ‖ tag (16)`, under the session key, associated data `"krowk/chunk/v1" ‖ the first nineteen bytes ‖ session id ‖ epoch ‖ SHA-256 of the previous sealed chunk`. At most 64 MiB, the registry's cap and the client's read limit alike. The final flag and the fence are bound, so a registry cannot mark a chunk final, unmark one, or move it to another lease.

A chunk sealed under a rotated key is **version 2**: version 1's first nineteen bytes, then the key epoch (4, big-endian), then the nonce, with all twenty-three bytes in the associated data labelled `"krowk/chunk/v2"` and the chain epoch derived from that epoch's key. Epoch 0 is only ever version 1, so an unrotated log is what it always was. A reader opens each chunk under the key its epoch names and refuses an epoch lower than the last chunk's: once a log has moved to a new key, a chunk under an older one — which a removed device could still seal — does not open. A reader holding a ring from before a rotation is told to open the session again.

**What the chain does not catch: a prefix.** A missing final chunk is normal while a session is still being written, so a hostile registry can serve any prefix of a live log — a reader sees a log that has not ended yet, which is what a live one looks like. Withholding a middle chunk shows as a gap; withholding the tail does not. What catches it is knowing the log's length from somewhere the registry cannot forge. **Ticket 19 does it in two places.** The session's sealed index and every checkpoint carry the latest chunk's index and digest (a log read past a known head must hold that head's own chunk at its index, not merely reach its length), and the host's welcome over the relay carries the head as the host sealed it; a reader holds the log to both and refuses one that stops short or reaches the head with another chunk (`harness.md` → Sync). The index alone cannot close it, since a registry can serve an older index with the log as it was, so what closes it is the host's live head and a head the device already read; a first attach with the host offline still trusts the registry for the log's length, and nothing else. What a chunk holds is `harness.md` → Sync's; the registry and R2 see its length and its digest.

Ticket 14 laid out the header; tickets 17 and 19 put this in the frames and the chunks.

## Signed session records

Nothing about a wrapped session key says who made it, and a registry colluding with any device key could serve one of its own. So the device that publishes a session signs its record with its Ed25519 key (`krowk_client::session_record`), over

```
"krowk/session-record/v1" ‖ session id (16) ‖ user key generation (4, big-endian) ‖ that generation's user key id (16)
  ‖ device list root (32) ‖ seal (1; 1 = user) ‖ the wrapped session key (74, or a rotated ring)
```

The root is the SHA-256 of the list's entry 0: whose sessions these are. The registry stores the signature and the signer's device id beside the wrapped key. A viewer, a listing, `--resume` or a host taking a session up again opens it only when:

- the seal is one this krowk knows, before anything else;
- the generation the wrapped key names is one the verified list reaches, and the key id signed over is the list's for it;
- the signer held that generation on the list — added at or before it and not removed or replaced before it (`Chain::signer_at`) — and the Ed25519 key checked is the list's, never one the record carries.

The registry's `owner_user_id` is not signed: the client can't learn it before publishing, and a value the registry supplies proves nothing against the registry. The root is the owner the signature binds.

**What it doesn't close.** A removed device can still sign a record under a generation it held, and the record verifies for reading, so with the registry's help it can pass off a session key of its own under that generation to a viewer. A host never takes such a record up (the signer must be listed now), and a session it does take up from before the removal is rotated to a key epoch the removed device does not hold (Key epochs, above). A registry serving a session's record from before a rotation gives a reader the old ring: the chunks after the rotation do not open for it, and the registry could serve it a tail sealed under the old key instead, which a reader holds off only with a head it already knows or the host's live one. A host served the old record rotates again to a key that does not open the log, and refuses to take it up.

## Nonces

Every XChaCha20-Poly1305 nonce is 24 bytes from the operating system's generator, drawn per message and carried with the ciphertext. No counter is kept: a counter would have to survive a crash and a restore from backup, and be coordinated between a host and whoever takes the lease next, and a repeated nonce under one key loses both confidentiality and integrity for those messages. At 192 bits a random nonce's chance of repeating is about 2⁻⁹⁷ after 2⁴⁸ messages under one key, which no session reaches. This is why the construction is XChaCha rather than ChaCha20-Poly1305, whose 96-bit nonce is not safe to draw at random at volume.

*This supersedes ticket 14's note in `harness.md`*, which expected ring's ChaCha20-Poly1305 with a per-connection message counter. The key is per session, not per connection, and outlives any connection, so a per-connection counter does not hold; the random 24-byte nonce does.

The counter orders messages; it never feeds the nonce, so a counter that restarts under a new epoch cannot repeat a nonce.

HPKE's nonces are its own: each wrap is a single-shot seal (sequence number 0) under a key schedule derived from a fresh ephemeral key.

## The recovery kit

The kit is 128 random bits from the operating system, shown as 12 words: BIP-0039's encoding — the bits, then the first 4 bits of their SHA-256 as a checksum, cut into 12 groups of 11 bits, each an index into BIP-0039's English list — and nothing else of BIP-0039: no PBKDF2, no passphrase (`phrase::encode_kit`, `recovery::RecoveryKit`). 128 bits matches X25519's and Ed25519's security level. A word not on the list is refused naming its position; a list word typed wrong fails the checksum 15 times in 16, and the rest derive a recovery device that isn't on the chain.

**The kit is not the user key.** It derives the recovery device (above), which is wrapped to on every rotation like any device, and signs like one: its own `add` at seq 0 or in `rotate-recovery`, the `rotate-recovery` that replaces it, and the entries of `krowk sync recover`. So whoever has the words has every session the recovery device can open, which is every session; replacing the kit changes nothing sealed; and a rotation never makes it stale.

It is offered at `krowk sync init` and can be skipped, with a reminder on every device until one exists. That departs from R-E2E-4's "mandatory": a person who skips has no recovery device, and losing every device loses every session. The words go to the terminal's stderr or to a `0600` file (`--save`), never to stdout, `--json` or a log. Showing, checking, replacing and recovering with it are `devices.md` → Recovery and → Replacing the kit; the CLI commands are being built.

Losing every device and the kit loses every private session. No one can decrypt them, krowk included, and there is no server-side reset.

## Adding a device

A new device, **B**, is added from one already on the person's list, **A**, by a short code A shows and the person types on B (R-E2E-3; the flow is `devices.md` → Adding a device). Under the code is SPAKE2, so the registry carrying the messages has nothing it can test a guess against offline. Built as `krowk_client::pairing`, sans-IO, driven by `krowk devices add` and `krowk sync join` over the registry's pairing rows.

**The code.** Eight symbols from `23456789ABCDEFGHJKMNPQRSTVWXYZ` — Crockford's base32 less `0` and `1`, so none of `0 O 1 I L U` — drawn from the operating system by rejection sampling (bytes under 240, mod 30), so every symbol is equally likely: 30⁸, about 2³⁹. Shown `XXXX-XXXX`; case, spaces and dashes are ignored when typed, and anything else is refused. The canonical form, upper case with no separators, is the SPAKE2 password. The code is never compared with anything but through SPAKE2.

**The identities.** Each side's is `"krowk/pair/v1" ‖ peer kind (1) ‖ user id (u16 length ‖ UTF-8, 1–64 bytes) ‖ device id (16)`: A's with A's device id, B's with B's. Peer kind 1 is another device of the same person. Kind 2 is reserved for a workspace invite (Harness ticket 32), which will bind both people's user ids and the workspace; the kind byte comes before any variable field, so it can define its own fields under the same label. A transcript therefore can't be moved to another person or another pair of devices.

**The transcript** both sides hash, `T = SHA-256("krowk/pair/v1" ‖ u16-prefixed identity A ‖ u16-prefixed identity B ‖ A's SPAKE2 message (33) ‖ B's (33) ‖ B's X25519 key (32) ‖ B's Ed25519 key (32) ‖ B's name (u16 ‖ UTF-8, ≤ 128 bytes) ‖ B's OS (u16 ‖ UTF-8, ≤ 64 bytes))`. A SPAKE2 message is its side byte and a point. The name and OS bounds here are the pairing's; an entry on the device list holds a name to 64 bytes and an OS to 32, and allows an empty OS where pairing doesn't, so the CLI holds a device to the list's bounds before it pairs — otherwise a name the person approved would fail to go on the list.

**The keys.** HKDF-SHA256 with no salt over SPAKE2's shared key gives two 32-byte keys, one per `info`: `krowk/pair-confirm/v1` for every MAC, and `krowk/pair-seal/v1` for the reply, so no MAC is made under a cipher's key. Each MAC is HMAC-SHA256 under the confirm key over `label ‖ T ‖ extra`, checked in constant time, with a label per message so none can stand in for another.

**The messages.** Every one starts `version (1) = 1 ‖ type (1)`:

| # | from | type | body |
|---|---|---|---|
| 1 | B → A | 1 hello | B's device id (16) ‖ B's SPAKE2 message (33) |
| 2 | A → B | 2 | A's SPAKE2 message (33) |
| 3 | B → A | 3 confirm | B's X25519 key ‖ Ed25519 key ‖ name ‖ OS ‖ MAC `"krowk/pair/v1 b-confirm"`, extra empty |
| 4 | A → B | 4 reply | nonce (24) ‖ XChaCha20-Poly1305 under the seal key of `payload ‖ MAC "krowk/pair/v1 a-reply"` with extra `SHA-256(payload)`; associated data `"krowk/pair/v1 reply" ‖ 0x01 ‖ 0x04 ‖ T` |
| 5 | B → A | 5 ack | MAC `"krowk/pair/v1 b-ack"`, extra `SHA-256(message 4)` |

A checks B's MAC, and that B's X25519 key hashes to the device id in the hello, before the person is asked anything; the prompt names only what the MAC covered. The payload is the caller's: the signed chain with the new `add` entry, the user key wrapped to B, and A's signing key. B hands the payload out only once A's MAC checks; the module doesn't look inside it, so B's caller (`krowk sync join`, through `pairing_payload`) must then check that A's key signed the new entry and that the entry adds exactly B's two keys, and keep nothing before that. That authenticated chain is where B's first pin comes from. A's caller posts the `add` and the wrap once the reply has gone out, whether or not B's ack checks: the reply carries the user key, so a machine that may hold it is listed and can be removed (devices.md → Adding a device). A verified ack (`Paired`) is what lets the pairing complete; without one, it ends `pairing_unconfirmed` with B listed. Should the post itself fail, acked or not, it ends `pairing_unconfirmed` saying B may hold the key unlisted. A pairing that dies before the reply leaves nothing on the list.

**One guess a side.** Every state is consumed by the step that leaves it, and any failure — a wrong MAC, a malformed message, a dropped connection — returns nothing to go on with and reads the same, "the pairing failed". Playing man in the middle, the registry gets one online guess posing as B to A, which kills A's pairing, and one posing as A to B, since B's confirm MAC lets it test one code offline. B's `start` takes the code by value, and the code can be neither cloned nor compared, so one code starts one pairing; the CLI must never start again with a code the person typed. That bounds a hostile registry at 2 in 30⁸, about 2⁻³⁸. The registry never picks the code, and routes B to the person's one open pairing by B's sign-in, so a pairing can't be reached from another account.

**Both commands need a person at a terminal.** `krowk devices add` asks a `[Y/n]` naming the new device, and both refuse to run off a terminal: krowk is run by agents, and a page or a repository an agent reads can tell it to run `krowk devices add`; a pairing no person agreed to is not a check. Debug builds let the test suite answer; release builds cannot.

**What the registry sees of a pairing**: the hello and the confirm in the clear — B's device id, public keys, name and OS, all of which it records once the device is added anyway — and the reply's and ack's lengths.

**Revocation binds.** A revoked or removed device is refused on every call that acts as it: every such call is signed by the device's own key (below), so naming a device is not a way to act as it.

## Signed registry requests

An API key says only which workspace, and which person, is asking; a CI job and every device hold the same kind of key. So every registry call that acts as a device is also signed by that device's Ed25519 signing key (`relay.md` → Why a signing key), which never leaves the machine. That turns the API key from a credential that can stand in for any device into one that says only who is asking. The calls are:

- appending to the device list and posting the user key's wraps, and A's side of a pairing: opening it and replying (ticket 16's registering and approval calls, `POST /v1/devices` and `/v1/device_approvals`, are retired with `410`);
- the lease: acquire, renew and hand over, release (`POST`, `PUT`, `DELETE /v1/sessions/:id/lease`);
- writing a session and its sealed index (`PUT /v1/sessions/:id`);
- declaring and finalizing a chunk (`POST /v1/sessions/:id/chunks`, `PUT …/chunks/:index/finalization`);
- a viewer's relay ticket (`GET /v1/sessions/:id/relay_ticket`), and so, with the lease calls, every relay ticket.
- reading sessions: listing them, showing one, listing a session's chunks (`GET /v1/sessions`, `/v1/sessions/:id`, `…/chunks`). The key alone, copied off a machine without its signing key, reads none of them, not even their sizes and times.

Each carries three headers beside the bearer key:

| Header | Value |
| --- | --- |
| `X-Krowk-Device` | the device's id, 32 hex characters |
| `X-Krowk-Timestamp` | when it was signed, Unix milliseconds, decimal |
| `X-Krowk-Signature` | pure Ed25519 (RFC 8032), 64 bytes in hex |

The signature is over five lines joined by `\n`:

```
krowk/registry/v1
METHOD
/v1/path?query          the request target, exactly as sent
TIMESTAMP               the header's digits, verbatim
hex(SHA-256(body))      of the empty string for a call with no body
```

None of the fields can hold a newline — a method is a token, a request target has none, the timestamp is digits and the digest hex — so the lines read one way only. The label keeps a registry signature from ever being a relay join's, or the reverse.

The registry refuses, each as a `401`:

- a call with any of the three missing (`signature_required`);
- a signature that does not verify under the signing key on record for that device of the key's person, or a device the person does not have (`signature_invalid`);
- a timestamp more than five minutes from the registry's clock (`signature_stale`);
- a signed request it has already accepted (`signature_replayed`): each one it accepts is remembered, by a digest of the device and the lines, in a table with a unique index until its window closes, so two processes cannot both accept one.

And then, for the device that signed: a revoked one is refused (`403 device_revoked`); a call that names another device in its body (`device`, for an acquire or a ticket) is refused (`403 device_mismatch`); and a write that is the lease holder's is refused unless the holder signed it (`409 lease_stale`), so a lease token copied to another device writes nothing. A hand-over is signed by the holder and names the next one. The signing key on record is the one the device-list entry that added the device names.

An Ed25519 signature is deterministic, so two requests of the same shape a device signs in the same millisecond carry one signature, and the second is refused as a replay. Within one process that never happens: each request it signs takes the clock's millisecond or one past the last it used, whichever is later, so a sync bridge and another reader of its session in the same process sign two; a stamp more than a second past the clock means the clock was set back, and the clock is taken again. Between two processes on one machine it still can; a read refused so is signed again after a moment with jitter, so the two do not meet again, and a write never is. The client signs afresh for every attempt and every redirect hop, in `krowk-api` through a signer `krowk-client` supplies (`e2e::DeviceSigner`); the key and the signature stay in `krowk-client` (R-E2E-2). A machine whose clock is more than five minutes out cannot act as its device until the clock is corrected.

## What the server side can and cannot see

It **cannot** see: the content of any session — prompts, answers, tool calls and their output, file contents, diffs, costs inside the log; the user key in any generation, any session key or any device's private key; the recovery kit. It cannot change a sealed payload, move it to another session, direction or connection, or replay or reorder it within one, without it failing to open; it can drop messages, and a dropped tail is detected only by the missing final sealed message (above). It cannot forge a session key or a user key wrap that an honest device accepts under the checks above, nor a device-list entry: every one is signed by a device on the list.

It **can** see, and this is the metadata the design leaks:

- **That a person syncs, and their device list whole**: every entry in the clear — each device's kind, public keys and id, its name and OS, every add, removal and kit replacement, who signed each and when by the signer's clock — plus each generation's user key id, the fixed-size wraps and which device each is for, and when each device was last seen. The pairing's hello and confirm carry the new device's keys, name and OS in the clear too. **Each device's name** is by default the machine's host name, so a person can tell their devices apart in `krowk devices list`. A host name can say whose machine it is; it is named here as the leak it is, and `--name` or `KROWK_DEVICE_NAME` sends another name instead. Names may hold no control characters, and the CLI strips any a registry sends back before printing.
- **That a device takes push notifications, and when it was rung**: its APNs token and whether it is a sandbox or production build, dropped when the device stops acting. Because a session's host asks the registry to ring the person's other devices, also **when** a session needed an approval or finished a turn, which of the two, and which of the person's devices it rang (it keeps the kind and time for a day, and which devices in its job queue and logs). Apple sees the app's bundle id, the device token, the session's id, the kind of event and the alert's fixed text. Nothing from the transcript reaches the registry's request or Apple.
- **That a device runs a krowk daemon, and when**: each device's announcement of itself (`harness.md` → Sync → Hosts announce themselves), its size rounded to 1 KiB, and when it was refreshed or removed, so when the daemon was up. Not its name, models, directories or sessions, which are sealed under the user key and bound to the device.
- **That one device sent another a command, and when it was answered**: each command another of the person's devices sends a host (`harness.md` → Sync → Starting a session from another device) — which device sent it to which, when, its size, and when it was answered, and the answer's size. Never its content: the directory, the model, the prompt and the session it started are sealed under the user key, bound to the command's id and both devices.
- **How many sessions there are**, their ids, when each was created and was last written, and which device held each session's lease.
- **Sizes**: every frame's and chunk's length, and so the rough size of each turn and of the whole session. Content is compressed before it is encrypted, so a length also reflects how compressible the content was. An attacker who can put chosen text into a session (a web page a tool fetched) and watch the lengths of the batches it lands in could in principle learn something about other text compressed in the same batch, as in CRIME. Nothing is padded; this is accepted for now, and `threat-model.md` → Accepted risks says why.
- **Vintages**: which ISO weeks a workspace has archived sessions in, when each week's vintage was replaced, and its length — not which sessions it holds or how many.
- **Timing**: when a session is active, how fast tokens arrive (a live batch goes every 16 ms while an answer streams), when devices attach and from which addresses.
- **What a session publishes, beside the file**: an artifact a session's `publish` pushes carries, as plaintext metadata, the session's id, the engine that ran the turn, the model and its provider (`krowk.session`, `krowk.engine`, `gen_ai.request.model`, `gen_ai.system` — `metadata.md`), so its card can link back to the session (R-EVID-2). Nothing from the transcript. Publishing is the person's or the agent's choice per file, and a public artifact shows these to anyone with the link; for an `openai-compatible` instance the model id is whatever the user configured, which can itself say something.
- **The frame header in the clear**: its kind (batch, frame, ack), its flags (whether compressed), its `enc` byte, the session id and `seq` — the relay routes and ring-buffers by them without decrypting (R-RELAY-1).

A hostile server can also deny service — refuse, delay, or drop. A dropped message shows as a gap in the counter, which opens and is visible; a stream cut short shows as a missing final sealed message. Neither is detected from `seq`, which repeats.

Anything the registry lists for a session — a title, a model, a cost, a working directory — is content, and is sealed like the rest; a field that is not sealed is a leak to name here before it ships.

## Threats to the device list

Two attackers matter here, and the defenses differ. A **hostile registry** can serve, withhold and store anything; the chain, the pins and the key ids are what stop it. A **thief** holds one of the person's devices — its keys, its API key, maybe its open browser — but not the person's password, their Google account or the kit; the fresh sign-in is aimed at the thief alone. What each can and can't do:

- **Guessing the pairing code.** The registry gets one online guess per side of each pairing, 2 in 30⁸ (Adding a device, above), and a wrong one ends that side. The bound holds only while the CLI never restarts B with a code already typed.
- **Rolling the list back.** A pinned device refuses a list shorter than its pin or forked from it, so the registry can't serve a list from before a removal; it can freeze a device at its pin by withholding what came after. That's denial of service. It is a confidentiality gap too, unless the frozen device refuses to seal: a device that doesn't know about a removal would seal under the removed device's generation. So every device verifies the list before sealing anything new, and refuses to seal under a generation older than the list names. A fresh machine has no pin: **recovery is the one deliberate weakness**. A fabricated chain can't carry the person's user key, since no device of theirs wrapped to it, and the kit's own signature is required wherever it is added, so a fork opens nothing and at worst makes recovery fail. An older genuine chain does open, and brings back a device that was removed; the person's review of every device by name, before anything is wrapped, is the check against it (`devices.md` → Recovery).
- **Wrapping a key of its own.** HPKE Base doesn't authenticate the wrapper, so the registry can wrap any key to any device. A device adopts only the key whose id the verified list names, and only if it opens back down to the key it already holds (The key hierarchy, above).
- **Racing a removal.** Any device on the list can sign a removal, so a thief acting before the person would remove the others and rotate the key to itself alone. Two things stop it: every destructive entry needs a fresh sign-in, which the thief doesn't have, and the kit can't be removed at all, only replaced, and only the kit itself signs its replacement, so it can always sign the thief's removal. Until a removal lands, the thief reads whatever is sealed under the current generation, and what a device held before it was removed it may have copied: removal promises nothing about the past. The dashboard's Revoke is a registry-side refusal only, since the web can't sign the chain, and a removal still has to follow from a device or the kit. A registry that stores one entry the clients refuse would freeze the list and with it the thief's removal, which is why the registry runs the clients' rules against the shared corpus (The device list, above).
- **Dislodging the kit.** Only the current kit replaces the kit. The one other way is starting over, which needs a fresh sign-in: a thief with the laptop and the password can do it and lock the owner out of the new chain, but it exposes nothing the laptop didn't already hold. A delayed kit replacement from a device was designed and cut, because that same attacker could already start over at once.
- **The fresh sign-in** — the password, or a Google sign-in with `prompt=login`, in the last five minutes — gates `sync init` and a start-over, a removal, a kit replacement, and the dashboard's Revoke. It defends against a thief only; a hostile registry could accept anything, and against it the chain and the pins are what count. A thief with the person's password as well has everything the person has but the kit.

## Zeroization

Every secret type in `krowk-client` — the account key, every user key generation, session keys, the recovery kit and the keys it derives, the pairing code and the keys derived from SPAKE2's, the device private key (x25519-dalek's `StaticSecret`, built with `zeroize`), HPKE's key schedule, the XChaCha cipher state, the kit's words and the buffers they are built and parsed in, the bytes of a key file the keystore reads from its storage and of the device, signing and account files it serialises into it, and the hex a device or signing key is written in and read from (built in one buffer of its exact size, so no earlier copy is left behind) — is wiped when dropped; the CLI writes each word straight to the terminal rather than building a copy of the rows. What is not, and is accepted: a key file `serde_json` rejects partway through, whose fields read so far it drops unwiped, and one hand-edited to spell its key with JSON escapes, which `serde_json` decodes through a scratch buffer of its own; the line editor's buffer while the kit is typed; on the phone, the copies UniFFI makes to carry a key item's bytes to and from the app's `KeyStorage` — it serialises each `Vec<u8>` into a buffer of its own, freed unwiped, which krowk cannot reach — and what the Keychain itself holds, which is the platform's (harness.md → Native clients: krowk-ffi, R-FFI-3); on the iPhone too, the `Data` a slot reaches Swift in to be sealed and the `Data` `AES.GCM.open` returns it in, which UniFFI copies and frees, and the Keychain's own copies of the sealed item: Swift keeps no plaintext of its own and wipes none, and CryptoKit wipes the shared secrets and AES keys it derives (R-IOS-8); the terminal's own scrollback, which keeps the displayed kit until it is cleared; and anything the operating system swaps or dumps. And in pairing, what the `spake2` crate holds — its copy of the code and its two scalars — and the HKDF and HMAC states there, which have no `Zeroize`, and the QR code `krowk devices add` draws of the invite (devices.md → Adding a device), whose encoder's buffers hold the URI and so the code: a memory disclosure during a pairing could show them, and the code is single-use and dies with the pairing, so that is accepted rather than worked around.

## Out of scope, and not yet

- **An audit.** Scoped in `threat-model.md`; performed in Harness P2 ticket 37 (R-OSS-2).
- **Platform key stores.** The seam they plug into exists (`KeyStorage`, above, R-CLIENT-4), and the terminal's is still files: the device key is a `0600` file in krowk's home, readable by anything running as the user. The iPhone's store is built: the Keychain behind a Secure Enclave key (R-IOS-8, above). What it does not close is code running inside the unlocked app, or a debugger attached to it, which can ask the enclave to open any slot, as the app itself does; and a slot's plaintext in the app's memory while Rust uses it (Zeroization). The macOS keychain and Android's keystore come with the desktop and Android clients, each an implementation of the seam that keeps no rule of its own.
- **Chunks after a downgrade.** A workspace that falls to the free plan has every artifact rescheduled to expire after the 30-day cancellation grace, and a session's chunks are artifacts: the synced log goes with the rest, unless the plan returns inside the window (`pricing.md` → Cancellation). The devices keep their local sessions.
- **What a removed device already held.** Removal rotates the user key, so nothing sealed afterwards opens on the removed device (`devices.md` → Removing a device); what it held before, it may have copied, and no rotation takes that back. Session keys are not rewrapped on rotation: an old session stays under the generation it was sealed with, which the newest opens.
- **Forward secrecy.** Session content is sealed under a long-lived session key, not a ratchet; a session key that leaks exposes that session, past and future.
- **Local malware, and a compromised client build.** A process running as the user, or a krowk binary that is not the one released (R-OSS-3's reproducible, signed builds are the answer: `threat-model.md` → A compromised build or dependency), sees everything the client does.

## Change log

- **2026-10-26** — Harness P4 ticket 136: a host command is a `start` or a `take`; what a `take` delivered again does.
- **2026-10-25** — Harness P4 ticket 124: host commands and their answers (R-HOST-4), sealed to the person's devices and bound to the command id and both devices; what the binding does not stop, and a command delivered again after the host restarts.
- **2026-10-25** — Harness P4 ticket 124: What the server side can and cannot see gains that one device sent another a command, when, its size, and when it was answered — never its content (R-HOST-4).
- **2026-10-25** — Harness P4 ticket 122: What the server side can and cannot see gains that a device runs a krowk daemon, and when — its announcement's rounded size and when it was refreshed or removed, not its name, models, directories or sessions (R-HOST-3).
- **2026-10-24** — Harness P4 ticket 120: records sealed to the person's devices (`seal_to_person`, a vintage's construction under a fixed label), and a host's announcement (R-HOST-3), the first of them; the opener's generation floor.
- **2026-10-20** — Harness P4 ticket 68: `krowk devices add` draws the pairing invite as a QR code on a terminal (devices.md → Adding a device). The invite URI, `krowk://pair?v=1&code=…&user=…&device=…`, carries only the code and two public ids, is parsed strictly by `krowk_client::pairing::invite`, and gives a phone what it must check — the person and the paired device — before it spends the code (the app's scanner is still to come). The code, the pairing and their checks are unchanged.
- **2026-10-20** — Harness P4 ticket 82: R-IOS-8 recorded — on the iPhone every key slot is a Keychain item (`com.krowk.keys`, `WhenUnlockedThisDeviceOnly`, not synchronizable) sealed under a non-exportable Secure Enclave P-256 key (ECDH, HKDF-SHA256, AES-GCM with the slot's name as authenticated data), no biometry on reads, a first-launch purge keyed on a marker file kept out of backups, a software key on the simulator only; platform key storage, not a second crypto implementation. Platform key stores: the iPhone's is built; what it leaves open.
- **2026-10-20** — Harness P4 ticket 113: the pairing's rule for what reaches the list stated as built. Once the reply carrying the user key has gone out, the paired device posts the `add` and the wrap whether or not the new device acknowledges, so a machine that may hold the key is listed and can be removed (a failed post is reported as such); a missing ack ends the pairing `pairing_unconfirmed`. Only a pairing that dies before the reply leaves nothing. Behaviour unchanged; the text had said the ack came first.
- **2026-10-19** — Harness P4 ticket 78: on the phone the keystore's storage is the app's `KeyStorage` over the Keychain (R-FFI-3); the copies UniFFI makes carrying a key item's bytes across the FFI are freed unwiped, out of krowk's reach, and join the accepted gaps in Zeroization.
- **2026-10-19** — Harness P4 ticket 65: the pairing reply's contents are built and checked in `krowk_client::pairing_payload` (devices.md → Adding a device), so the phone clients to come run the new device's check that the chain adds exactly it, signed by the paired device, rather than re-implementing it (R-E2E-3, R-CLIENT-1). Format and checks unchanged.
- **2026-10-19** — Harness P4 ticket 63: R-CLIENT-4 recorded — the keystore keeps every key-storage rule and reaches storage only through the `KeyStorage` seam (`FileStorage` for krowk's home, `MemoryStorage` for tests); platform key stores plug into it rather than re-implementing a rule. The bytes a key file is read into and serialised into, and a key's hex, are wiped now, so `device.json`'s JSON text leaves the accepted gaps in Zeroization but for a file rejected partway through or hand-edited to spell its key with JSON escapes.
- **2026-10-09** — Harness P2 ticket 41: vintages sealed under the user key at the current generation, format 2 binding the generation and the user key's id; format 1, under the account key, refused by name; the device list verified before a vintage is sealed.
- **2026-10-01** — The 7-day kit replacement from a device cut, with its pending state, cancel, clocks, backdating rule and prefix verification: the attacker it held off could already start over. Only the current kit replaces the kit. A start-over carries the user key forward (`carried` at seq 0, generation 1 wrapping the old key). Entry time informational. The session key's version-2 wrap under the user key, and signed session records.
- **2026-09-30** — Devices you own (to-do D9): the account key, the 24-word phrase and approval by device id replaced by the built client design — the user key with generations (its id, both wraps, adoption only by the list's key id), the recovery kit as a derived device, the signed device list (encoding, signers, verifier rules, clocks, prefix, batches, the corpus), SPAKE2 pairing stated exactly (identities, transcript, keys, MACs, messages, one guess a side); `spake2`'s audit status (none; python-spake2's vector); the threats to the device list; the metadata the list and the pairing leak; the CLI and the registry named as being built.
- **2026-09-29** — Harness P2 ticket 28: the threat model is `threat-model.md`, with the accepted length leak and the audit's scope there; the forward references to ticket 28 point at it.
- **2026-09-29** — Harness P1 ticket 18: the approval code over both public keys and one waiting request per device key; the registry vouching for the signing key in relay tickets; the relay signing key required, set once and carried in the approval; what it authorizes (relay admission, not decryption); the interim exposure of a workspace API key until todo 16c signs registry writes.

- **2026-09-29** — Written with Harness P1 ticket 15: the constructions and the one suite, why RustCrypto and `hpke` over ring and `hpke-rs`, audit status, the key hierarchy (device key, account key, session keys), the files and blob formats, the frame header's `enc` = 1, random 24-byte nonces (superseding ticket 14's counter note), the recovery phrase as the key itself, what adding a device must check against a hostile server, the metadata the server sees, zeroization, and what is out of scope.
- **2026-09-29** — Ticket 15, review round 1: the frame header alone binds no position (it repeats), so content is sealed with the session, direction, the receiver's epoch and a strictly increasing counter (`Sealer`, `Opener`), with the rules tickets 17 and 19 follow and the stream's end authenticated by a final sealed message; the claims about reordering and `seq` corrected; the key id shown by `init` and `recover` for the 1-in-256 typo, and `recover` retried; pipe from a file, not `echo`; the Quarkslab report linked; the stored device id checked.
- **2026-09-29** — Ticket 15, review round 2: per-receiver epochs cannot serve the relay's ring buffer to a late client; ticket 17 takes a host-chosen per-stream epoch announced sealed to each receiver and accepted once.
- **2026-09-29** — Harness P1 ticket 16: adding a device built — the new device's id typed in full on the approving device, the account key's id given on the new one, the `join` origin, the registry's per-workspace account key id that catches a mistyped phrase at `recover`; the sealed session index and its format; what the registry holds of a wrapped key.
- **2026-09-29** — Ticket 16, review round 1: `approve` and `join` need a person at a terminal and a yes; the mismatch refusal never points at `join`; the owner's reset of the pinned account key id; revocation stated as advisory until requests are signed; `--name` for the device name.
- **2026-09-29** — Ticket 16b: chunks at rest built — the index as the counter, the epoch derived from the session key, the final flag bound, the reader strictly in order; the sealed chunk's format.
- **2026-09-29** — Ticket 16b, review round 1: the log is a chain (each chunk binds the previous one's digest) and carries its lease fence; a prefix of a live log is not detected at rest, and what will detect it; chunks expire with the rest after a downgrade; chunks at most 64 MiB.
- **2026-09-29** — Harness P1 ticket 17: each device's Ed25519 signing key for joining a relay; the host-chosen per-stream epoch taken for host → client, as `relay.md` builds it.
- **2026-09-29** — Harness P1 ticket 19: sealing across the relay (the stream's counter and `seq`, the routed chain under the viewer's challenge, the hello's derived epoch, streams accepted once, the final flag), and the prefix caught by the host's live head and the head in the sealed index and each checkpoint.
- **2026-09-29** — Harness P1 ticket 16c: signed registry requests — every call that acts as a device signed by its Ed25519 key over the method, target, timestamp and body digest, with ±5 minutes of skew and a replay memory; revocation binds; relay tickets minted only on the device's own signed request; the interim exposure closed.
- **2026-09-29** — Harness P2 ticket 24: vintages — a fresh vintage key per vintage, wrapped under the account key inside the vintage's blob, both layers binding the week; the format; what an older vintage of the same week costs; the weeks and lengths the registry sees.
- **2026-10-04** — Per-session key epochs: a host taking up a session sealed before a device's removal adds a key epoch first; the ring's version-3 blob, version-2 chunks naming their epoch and never going back to an older one, and the registry's rotation rule.
