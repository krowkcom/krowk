import Foundation

/// What the phone says to a session it follows: the three commands M1's
/// composer sends (ticket 90), and the answer to an approval (ticket 91).
/// KrowkAccount builds the generated `Command` from it (R-CLIENT-1: never
/// the wire JSON here), for this session, with no model, toolset, effort
/// or budget and the default permission mode; the host's bridge runs a
/// remote prompt under the session's own settings anyway. An answer names
/// its request's own session.
public enum LiveCommand: Equatable, Sendable {
    /// A new turn, with this text.
    case prompt(String)
    /// This text added to the running turn, which goes on.
    case steer(String)
    /// The running turn stopped, keeping what it produced.
    case interrupt
    /// An approval answered: allowed once or for the session, or denied
    /// (with the answers to its questions).
    case approve(ApprovalReply)
}

/// The composer's commands as the screen shows them until the session's
/// log takes them over (R-LAG-8, R-IOS-5): a bubble per prompt or steer,
/// and "stopping…" while an interrupt is out. Pure bookkeeping, no IO:
/// the core (krowk-ffi's `Viewer`, its command book) mints the ids,
/// queues with no host, resends after a reconnect and has the host run
/// each once; this only follows what it reports.
///
/// Each command gets a `token` here, at the tap, before anything crosses
/// into Rust, so its bubble shows in the same run-loop turn. The core's id
/// for it arrives as `minted`, then `sent(id, queued)`, then
/// `acked(id, error)`. A prompt or steer the host logged comes back as a
/// `userText` item: `logged` drops the oldest live bubble with that text
/// (FIFO, so two identical prompts reconcile one each), and the logged row
/// takes its place. A bubble the host refused stays, with the reason,
/// until the person edits and resends it (a new command) or dismisses it.
public struct PendingCommands: Equatable, Sendable {
    public enum Kind: Equatable, Sendable {
        case prompt
        case steer
        case interrupt
        /// An approval's answer: no bubble; its ack goes to the approval
        /// queue (`Answered.approval`).
        case approve
    }

    public enum State: Equatable, Sendable {
        /// Shown, and sent (or about to be) to a host that welcomed this
        /// phone; not yet acked.
        case sending
        /// Held by the core for the next host to welcome this phone.
        case queued
        /// Acked: the host took it. Its logged item takes over.
        case settled
        /// Refused by the host (its ack's error), or never sent: why.
        case refused(String)
    }

    public struct Entry: Equatable, Sendable {
        public let token: UInt64
        public let kind: Kind
        public let text: String
        /// The core's id, `<device>-<run id>-<counter>`, once minted.
        public internal(set) var id: String?
        public internal(set) var state: State

        /// Sent or queued, and not answered: the core still holds it.
        public var isOut: Bool { state == .sending || state == .queued }

        /// A prompt or steer: shown as a bubble.
        public var isBubble: Bool { kind == .prompt || kind == .steer }
    }

    /// In the order typed.
    public private(set) var entries: [Entry] = []
    private var next: UInt64 = 0

    public init() {}

    /// A command typed: shown at once, as `sending` (`queued` once the
    /// core says so). Its token.
    @discardableResult
    public mutating func add(_ kind: Kind, text: String = "") -> UInt64 {
        next += 1
        entries.append(Entry(token: next, kind: kind, text: text, id: nil, state: .sending))
        return next
    }

    public func entry(_ token: UInt64) -> Entry? {
        entries.first { $0.token == token }
    }

    /// The core gave command `token` the id `id`.
    public mutating func minted(_ token: UInt64, id: String) {
        guard let at = entries.firstIndex(where: { $0.token == token }) else { return }
        entries[at].id = id
    }

    /// `Update.sent`: shown sent, or queued with no host. Only moves an
    /// unanswered command; the token it changed.
    @discardableResult
    public mutating func sent(_ id: String, queued: Bool) -> UInt64? {
        guard let at = entries.firstIndex(where: { $0.id == id }), entries[at].isOut else { return nil }
        entries[at].state = queued ? .queued : .sending
        return entries[at].token
    }

    /// What an ack did.
    public enum Answered: Equatable, Sendable {
        /// A prompt or steer: settled, or refused with the host's sentence.
        case bubble(token: UInt64)
        /// An interrupt answered and gone: `error` when the host refused it.
        case interrupt(token: UInt64, error: String?)
        /// An approval's answer acked and gone: `error` when the host
        /// refused it.
        case approval(token: UInt64, error: String?)
    }

    /// `Update.acked`. An id with no entry here (its bubble already gave
    /// way to its logged item, or another viewer run's) changes nothing. A
    /// steer taken after the turn it was meant for ended (`turnRunning`
    /// false) won't be read: refused, as `turnEnded` refuses one.
    public mutating func acked(_ id: String, error: String?, turnRunning: Bool = true) -> Answered? {
        guard let at = entries.firstIndex(where: { $0.id == id }) else { return nil }
        let entry = entries[at]
        if entry.kind == .interrupt {
            entries.remove(at: at)
            return .interrupt(token: entry.token, error: error)
        }
        if entry.kind == .approve {
            entries.remove(at: at)
            return .approval(token: entry.token, error: error)
        }
        guard entry.isOut else { return nil }
        if let error {
            entries[at].state = .refused(Self.refusal(error))
        } else if entry.kind == .steer && !turnRunning {
            entries[at].state = .refused(Self.unread)
        } else {
            entries[at].state = .settled
        }
        return .bubble(token: entry.token)
    }

    /// Command `token` never reached the core (its connection had
    /// stopped): refused, saying `why`. An interrupt or an answer just goes.
    public mutating func unsent(_ token: UInt64, why: String) {
        guard let at = entries.firstIndex(where: { $0.token == token }) else { return }
        if !entries[at].isBubble {
            entries.remove(at: at)
        } else if entries[at].isOut {
            entries[at].state = .refused(why)
        }
    }

    /// The connection stopped for good: nothing still out will be
    /// answered. Its bubbles are refused, saying so (a sent one may have
    /// run; a queued one did not), and interrupts and answers go (the
    /// approval queue says what became of those). The tokens changed.
    @discardableResult
    public mutating func connectionEnded() -> [UInt64] {
        var changed: [UInt64] = []
        entries.removeAll { !$0.isBubble }
        for at in entries.indices where entries[at].isOut {
            // Queued, or never given an id by the core: it never went.
            entries[at].state = .refused(entries[at].state == .queued || entries[at].id == nil
                ? "Not sent: the connection to this session stopped. Open the session again to send it."
                : "No answer from the host before the connection stopped: it may have run. Open the session again to see.")
            changed.append(entries[at].token)
        }
        return changed
    }

    /// A `userText` the host logged (live, or a catch-up's, never the
    /// session read at rest): the oldest prompt or steer bubble not refused
    /// whose text is this goes, and its token is returned. A queued one
    /// counts too: after a welcome its send and its logged item can come
    /// before the ack.
    public mutating func logged(_ text: String) -> UInt64? {
        // The live bubbles first; then a steer refused as unread at its
        // turn's end, which may yet be logged (a gap's catch-up brings it
        // after the end) — only once no live bubble has the text, so a
        // prompt retyped from it is the one its logged item takes.
        let live = { (entry: Entry) in entry.isBubble && !entry.isRefused }
        let unread = { (entry: Entry) in entry.state == .refused(Self.unread) }
        let match = entries.firstIndex { live($0) && $0.text == text }
            // A host that trimmed what it logged still matches.
            ?? entries.firstIndex { live($0) && Self.trimmed($0.text) == Self.trimmed(text) }
            ?? entries.firstIndex { unread($0) && Self.trimmed($0.text) == Self.trimmed(text) }
        guard let at = match else { return nil }
        return entries.remove(at: at).token
    }

    /// A turn ended: no interrupt is waiting any more ("stopping…" stays
    /// until the interrupt's ack or the turn's end, whichever first). A
    /// steer the host took and the turn never logged won't be read now: it
    /// is refused, saying so, to be sent again as a prompt. The steers'
    /// tokens.
    @discardableResult
    public mutating func turnEnded() -> [UInt64] {
        entries.removeAll { $0.kind == .interrupt }
        var unread: [UInt64] = []
        for at in entries.indices where entries[at].kind == .steer && entries[at].state == .settled {
            entries[at].state = .refused(Self.unread)
            unread.append(entries[at].token)
        }
        return unread
    }

    static let unread = "The turn ended before it read this: send it again as a prompt."

    /// A refused bubble kept from an earlier screen of the session (its
    /// connection let go): shown again, refused, with "Edit and resend".
    @discardableResult
    public mutating func addRefused(_ kind: Kind, text: String, why: String) -> UInt64 {
        next += 1
        entries.append(Entry(token: next, kind: kind, text: text, id: nil, state: .refused(why)))
        return next
    }

    /// A bubble the person took back (edit and resend) or dismissed.
    public mutating func remove(_ token: UInt64) {
        entries.removeAll { $0.token == token }
    }

    /// An interrupt is out: "stopping…".
    public var stopping: Bool { entries.contains { $0.kind == .interrupt } }

    /// Commands the core still holds unanswered: while there are any, a
    /// screen left keeps its connection (`ParkedScreens`).
    public var out: Int { entries.filter(\.isOut).count }

    /// The bubbles, in the order typed.
    public var bubbles: [Entry] { entries.filter(\.isBubble) }

    /// The host's ack error as the bubble says it: its sentence, or a
    /// plain one when it gave none.
    static func refusal(_ error: String) -> String {
        let said = error.trimmingCharacters(in: .whitespacesAndNewlines)
        return said.isEmpty ? "The host refused this." : said
    }

    static func trimmed(_ text: String) -> String {
        text.trimmingCharacters(in: .whitespacesAndNewlines)
    }
}

extension PendingCommands.Entry {
    var isRefused: Bool {
        if case .refused = state { true } else { false }
    }
}

/// What the composer lets through.
public enum ComposerRules {
    /// The most a prompt or steer may hold, in UTF-8 bytes: far below the
    /// relay's 4 MiB batch, which also carries the session's lines, and
    /// more than a pasted log needs.
    public static let maxBytes = 256 * 1024

    /// Why `text` can't be sent, or nil when it can: blank, or too long.
    public static func problem(_ text: String) -> String? {
        if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty { return "" }
        let bytes = text.utf8.count
        if bytes > maxBytes {
            return "Too long to send: \(bytes / 1024) KB, at most \(maxBytes / 1024) KB."
        }
        return nil
    }
}
