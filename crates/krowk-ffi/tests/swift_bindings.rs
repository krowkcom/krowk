//! R-PROTO-2, R-FFI-1: the Swift the phone compiles is generated from this
//! crate's Rust, never written by hand, and it generates on Linux from the
//! host build on every `make check`. Library mode, as the crate doc's
//! manual command: UniFFI reads the interface out of the cdylib cargo built
//! beside this test, and the module names out of `uniffi.toml`.

use std::path::PathBuf;

/// The host build's cdylib: in the `deps/` this test binary runs from,
/// else in `target/<profile>/` above it.
fn cdylib() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary's path");
    let deps = exe.parent().expect("the test binary is in deps/");
    let dir = deps.parent().expect("deps/ is in target/<profile>/");
    let name = format!(
        "{}krowk_ffi{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    // deps/ first: the build that made this test binary always writes it
    // there, while target/<profile>/ holds whatever a plain `cargo build`
    // (the crate doc's manual command) last copied up, which may be older.
    [deps.join(&name), dir.join(&name)]
        .into_iter()
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("{name} is built beside {}", deps.display()))
}

#[test]
fn r_proto_2_r_ffi_3_swift_bindings_generate_from_the_host_build() {
    let out = std::env::temp_dir().join(format!("krowk-ffi-swift-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    let utf8 = |p: PathBuf| {
        p.into_os_string()
            .into_string()
            .expect("a UTF-8 path")
            .into()
    };
    uniffi::generate(uniffi::GenerateOptions {
        languages: vec![uniffi::TargetLanguage::Swift],
        source: utf8(cdylib()),
        out_dir: utf8(out.clone()),
        // swiftformat is not on Linux.
        format: false,
        // Only the workspace's manifests: krowk_ffi's uniffi.toml is all
        // the generator needs, and the test stays offline.
        metadata_no_deps: true,
        ..Default::default()
    })
    .expect("uniffi generates the Swift bindings");

    for file in ["Krowk.swift", "KrowkFFI.h", "KrowkFFI.modulemap"] {
        assert!(
            out.join(file).is_file(),
            "{file} is generated (module Krowk over KrowkFFI, uniffi.toml)"
        );
    }
    // Unformatted, UniFFI's templates space a declaration their own way
    // (`parsePairingCode(text: String)throws  -> String`), so whitespace is
    // left out of the comparison.
    let squeeze = |s: &str| s.split_whitespace().collect::<String>();
    let swift = squeeze(&std::fs::read_to_string(out.join("Krowk.swift")).expect("Krowk.swift reads"));
    for decl in [
        "func protocolVersion() -> UInt32",
        "func parsePairingCode(text: String) throws -> String",
        "enum KrowkError",
        // R-FFI-3: the storage Swift implements over the Keychain, and the
        // sign-in and account that use it.
        "protocol KeyStorage",
        "func read(slot: String) throws -> Data?",
        "func write(slot: String, bytes: Data) throws",
        "func remove(slot: String) throws",
        "func code() -> String",
        "func verificationUrl() -> String",
        "func expiresAt() -> String",
        "func cancel()",
        "class SignIn",
        "static func start(api: String, deviceName: String) throws -> SignIn",
        "func wait(storage: KeyStorage) throws -> SignedIn",
        "struct SignedIn",
        "var workspaceName: String",
        "class Account",
        // `open` is a Swift keyword, so the generator quotes it; Swift
        // still calls `Account.open(api:storage:)`.
        "static func `open`(api: String, storage: KeyStorage) throws -> Account",
        "func deviceId() -> String",
        "func workspace() -> String",
        "func workspaceName() -> String",
        "func hasUserKeys() throws -> Bool",
        "func signOut() throws",
        // R-E2E-3: joining a pairing, and the scanned invite's code.
        "func joinPairing(code: String, name: String) throws -> Joined",
        "func cancelPairing()",
        "func joinInvite(invite: PairingInvite, name: String) throws -> Joined",
        "struct Joined",
        "var deviceName: String",
        "func parsePairingInvite(uri: String) -> PairingInvite?",
        "struct PairingInvite",
        "case Refused(code: String, detail: String)",
        "case Network(detail: String)",
        "case Decode(detail: String)",
        // R-PROTO-2: the protocol's types, generated from krowk-client's
        // through src/protocol.rs, and JSON only at the edges.
        "enum StreamLine",
        "case log(LogEvent)",
        "case live(LiveEvent)",
        "struct LogEvent",
        "enum LogBody",
        "case turnStarted(turnId: String, model: ModelRef,",
        "enum LiveEvent",
        "case itemDelta(sessionId: String, turnId: String, itemId: String, delta: Delta)",
        "case approvalRequested(ApprovalRequest)",
        "case result(RunResult)",
        "enum Command",
        "case interrupt(sessionId: String)",
        "struct ApprovalRequest",
        "enum ApprovalDecision",
        "struct Question",
        "struct QuestionAnswer",
        "enum Item",
        "enum Delta",
        "struct RunResult",
        "var worktree: KeptWorktree?",
        // serde_json::Value as its JSON text, usize as a u64.
        "typealias Value = String",
        "typealias Usize = UInt64",
        "func decodeStreamLine(json: String) throws -> StreamLine",
        "func decodeLogEvent(json: String) throws -> LogEvent",
        "func encodeCommand(command: Command) throws -> String",
        // R-CLIENT-1: the live viewer, its inputs and the effects Swift
        // carries out; the socket and the timers are Swift's.
        "class Viewer",
        "static func `open`(session: OpenedSession, nowMs: UInt64) throws -> Viewer",
        "func start(nowMs: UInt64) -> [Effect]",
        "func ticket(ticket: String, nowMs: UInt64) -> [Effect]",
        "func ticketFailed(detail: String, nowMs: UInt64) -> [Effect]",
        "func received(connection: UInt64, frame: WsFrame, nowMs: UInt64) -> [Effect]",
        "func closed(connection: UInt64, nowMs: UInt64) -> [Effect]",
        "func tick(nowMs: UInt64) -> [Effect]",
        "func command(command: Command, nowMs: UInt64) -> [Effect]",
        "func nextWakeMs() -> UInt64?",
        "func readChunks() -> ChunksRead",
        "func chunksRead(read: ChunksRead, nowMs: UInt64) -> [Effect]",
        "enum Effect",
        "case needTicket(atMs: UInt64)",
        "case dial(url: String, ticket: String, connection: UInt64)",
        "case send(bytes: Data)",
        "enum WsFrame",
        "enum ViewerUpdate",
        "case sent(id: String, queued: Bool)",
        "case acked(id: String, error: String?)",
        "class ChunksRead",
    ] {
        assert!(swift.contains(&squeeze(decl)), "Krowk.swift declares `{decl}`");
    }
    let _ = std::fs::remove_dir_all(&out);
}
