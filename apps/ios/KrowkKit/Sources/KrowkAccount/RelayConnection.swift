import Foundation
import Krowk
import KrowkKit
import Network
import os

/// One synced session followed live through the relay (R-CLIENT-1,
/// R-LAG-9, R-HAND-4): Swift owns the socket, Network.framework's
/// `NWConnection` with its WebSocket protocol, and the timers; krowk-ffi's
/// `Viewer` decides everything else — the join and its signature, the
/// heartbeat, when a link is dead, when and from where to join again, the
/// acks inside the relay's window. This does what its effects say, passes
/// in every message and close the socket sees, and keeps a status the
/// screen shows (`RelayStatus`).
///
/// An actor whose executor is its own serial queue, the one each
/// connection starts on: Network.framework's callbacks and the timers run
/// there, never on the main queue, and step into the actor with
/// `assumeIsolated`. The registry calls that block (opening the session,
/// a relay ticket, reading chunks) run on a thread of their own, never on
/// this queue or the concurrency runtime's, and what they return is fed in
/// back on the queue: a heartbeat is never queued behind them.
///
/// Updates for the screen go to `mailbox`, which the main thread drains
/// once per display frame. Nothing here logs: a relay ticket never leaves
/// the call that dials with it.
public actor RelayConnection: LiveSession {
    /// The relay's largest message, its header and a 4 MiB batch
    /// (`krowk_harness::relay`'s `max_message`): anything larger fails the
    /// connection, and the viewer joins again.
    static let maxMessage = 28 + (4 << 20)

    public nonisolated let mailbox = UpdateMailbox()

    private let queue: DispatchSerialQueue
    public nonisolated var unownedExecutor: UnownedSerialExecutor { queue.asUnownedSerialExecutor() }

    /// Opens the session from the registry: blocking.
    private let opening: @Sendable () throws -> OpenedSession
    private var session: OpenedSession?
    private var viewer: Viewer?
    /// The connection the viewer dialed last, and its number, until it is
    /// dropped or ends.
    private var connection: NWConnection?
    private var current: UInt64?
    /// When the viewer next wants `tick`.
    private var wake: DispatchSourceTimer?
    /// The `NeedTicket` acted on: a later one replaces it, and a ticket
    /// fetched for an earlier one is dropped.
    private var ticketAsk: UInt64 = 0
    private var started = false
    /// Stopped, or the viewer ended: nothing more is called.
    private var done = false

    private var tracker = RelayStatusTracker()
    private let shown = OSAllocatedUnfairLock<RelayStatus>(initialState: .connecting)
    private var onStatus: (@MainActor @Sendable (RelayStatus) -> Void)?
    private let counts = OSAllocatedUnfairLock<Int>(initialState: 0)

    /// `opening` opens the session to follow (`Account.openSession`), and
    /// blocks; it runs once, on a thread of its own, as `follow` starts.
    public init(opening: @escaping @Sendable () throws -> OpenedSession) {
        self.queue = DispatchSerialQueue(label: "krowk.relay")
        self.opening = opening
    }

    /// One never stopped (its screen let go of it) still closes its socket.
    deinit {
        connection?.stateUpdateHandler = nil
        connection?.cancel()
        wake?.cancel()
    }

    /// The status as last published.
    public nonisolated var status: RelayStatus { shown.withLock { $0 } }

    /// How many connections the viewer has dialed so far.
    public nonisolated var dials: Int { counts.withLock { $0 } }

    // MARK: LiveSession

    public nonisolated func follow(status: @escaping @MainActor @Sendable (RelayStatus) -> Void) {
        queue.async { self.assumeIsolated { $0.start(status) } }
    }

    public nonisolated func stop() {
        queue.async { self.assumeIsolated { $0.halt() } }
    }

    public nonisolated func takeFrame() -> Int {
        mailbox.drain().count
    }

    #if DEBUG
    /// Ends the current connection as a network drop would (`forceCancel`):
    /// the viewer hears it closed, and joins again at its cursor. For the
    /// tests.
    public nonisolated func dropConnection() {
        queue.async { self.assumeIsolated { $0.connection?.forceCancel() } }
    }
    #endif

    // MARK: Driving the viewer

    private func start(_ status: (@MainActor @Sendable (RelayStatus) -> Void)?) {
        guard !started, !done else { return }
        started = true
        onStatus = status
        publish()
        let opening = self.opening
        offQueue(queue) {
            Result { () throws -> (OpenedSession, Viewer) in
                let session = try opening()
                return (session, try Viewer.open(session: session, nowMs: Self.now()))
            }
        } then: { (me: isolated RelayConnection, opened) in
            me.opened(opened)
        }
    }

    private func opened(_ opened: Result<(OpenedSession, Viewer), any Error>) {
        guard !done else { return }
        switch opened {
        case .success(let (session, viewer)):
            self.session = session
            self.viewer = viewer
            let wake = DispatchSource.makeTimerSource(queue: queue)
            wake.setEventHandler { [weak self] in self?.assumeIsolated { $0.tick() } }
            wake.resume()
            self.wake = wake
            run(viewer.start(nowMs: Self.now()))
        case .failure(let error):
            done = true
            tracker.ended(Self.detail(error))
            publishIfChanged()
        }
    }

    private func tick() {
        guard !done, let viewer else { return }
        run(viewer.tick(nowMs: Self.now()))
    }

    /// Carries out the viewer's effects in order, then sets the timer for
    /// when it next wants `tick`.
    private func run(_ effects: [Effect]) {
        // The `Failed` this batch showed: why, if its `End` follows.
        var failure: String?
        for effect in effects {
            guard !done, let viewer else { break }
            switch effect {
            case .needTicket(let atMs):
                askTicket(at: atMs)
            case .dial(let url, let ticket, let connection):
                dial(url: url, ticket: ticket, number: connection)
            case .send(let bytes):
                send(bytes, .binary)
            case .ping:
                send(Data("ping".utf8), .text)
            case .close:
                drop()
                tracker.dropped()
            case .show(let updates):
                for update in updates {
                    switch update {
                    case .host(let present): tracker.host(present)
                    case .note(let text): tracker.note(text)
                    case .failed(let detail): failure = detail
                    default: break
                    }
                }
                mailbox.post(updates)
            case .readChunks:
                offQueue(queue) { viewer.readChunks() } then: { me, read in
                    guard !me.done, let viewer = me.viewer else { return }
                    me.run(viewer.chunksRead(read: read, nowMs: Self.now()))
                }
            case .end:
                tracker.ended(failure)
                halt()
            }
        }
        publishIfChanged()
        armWake()
    }

    private func armWake() {
        guard !done, let wake, let at = viewer?.nextWakeMs() else { return }
        let now = Self.now()
        // At least once a minute, whatever it asked: a clock read late
        // never leaves the viewer asleep.
        let wait = Int(min(at > now ? at - now : 0, 60_000))
        // Wall time, which runs on while the phone sleeps, as `now` does.
        wake.schedule(wallDeadline: .now() + .milliseconds(wait), leeway: .milliseconds(4))
    }

    /// `NeedTicket`: at `atMs`, the ticket from the registry, off the
    /// queue; then `ticket` or `ticketFailed`. The ticket goes from that
    /// call to the viewer and is never kept or logged.
    private func askTicket(at atMs: UInt64) {
        ticketAsk += 1
        let ask = ticketAsk
        let now = Self.now()
        let wait = Int(min(atMs > now ? atMs - now : 0, 60_000))
        queue.asyncAfter(wallDeadline: .now() + .milliseconds(wait)) { [weak self] in
            self?.assumeIsolated { me in
                guard !me.done, ask == me.ticketAsk, let session = me.session else { return }
                me.offQueue(me.queue) { Result { try session.relayTicket() } } then: { me, fetched in
                    guard !me.done, ask == me.ticketAsk, let viewer = me.viewer else { return }
                    switch fetched {
                    case .success(let ticket): me.run(viewer.ticket(ticket: ticket, nowMs: Self.now()))
                    case .failure(let error): me.run(viewer.ticketFailed(detail: Self.detail(error), nowMs: Self.now()))
                    }
                }
            }
        }
    }

    // MARK: The socket

    private func dial(url: String, ticket: String, number: UInt64) {
        drop()
        // TLS for wss (the system's trust, no exception); plain TCP only
        // for a DEBUG build's ws:// relay on this machine, the fake
        // fleet's. Anything else would send the ticket in the clear, and
        // never will work: the viewer stops, saying why.
        guard let to = URL(string: url), let secure = Self.secure(to) else {
            tracker.ended("the relay must be a wss:// address (ws:// only to this machine, in a DEBUG build)")
            halt()
            return
        }
        counts.withLock { $0 += 1 }
        current = number
        let parameters: NWParameters = secure ? .tls : .tcp
        let websocket = NWProtocolWebSocket.Options()
        websocket.autoReplyPing = true
        websocket.maximumMessageSize = Self.maxMessage
        websocket.setAdditionalHeaders([("X-Krowk-Ticket", ticket)])
        parameters.defaultProtocolStack.applicationProtocols.insert(websocket, at: 0)
        let connection = NWConnection(to: .url(to), using: parameters)
        connection.stateUpdateHandler = { [weak self] state in
            self?.assumeIsolated { $0.changed(state, number) }
        }
        self.connection = connection
        connection.start(queue: queue)
        receive(on: connection, number)
    }

    private func changed(_ state: NWConnection.State, _ number: UInt64) {
        switch state {
        // `waiting` is a connection that can't be made for now (refused,
        // no route): it fails like one, and the viewer tries again.
        case .failed, .waiting, .cancelled: ended(number)
        case .setup, .preparing, .ready: break
        @unknown default: break
        }
    }

    /// The next message, then the one after: one at a time, so a fast
    /// relay waits on this queue rather than filling memory.
    private func receive(on connection: NWConnection, _ number: UInt64) {
        connection.receiveMessage { [weak self] content, context, isComplete, error in
            // Never the main queue: feeding a 4 MiB batch there would
            // stall scrolling.
            dispatchPrecondition(condition: .notOnQueue(.main))
            self?.assumeIsolated { $0.received(content, context, isComplete, error, connection, number) }
        }
    }

    private func received(_ content: Data?, _ context: NWConnection.ContentContext?, _ isComplete: Bool, _ error: NWError?, _ connection: NWConnection, _ number: UInt64) {
        guard !done, current == number, self.connection === connection, let viewer else { return }
        if error != nil {
            ended(number)
            return
        }
        let metadata = context?.protocolMetadata(definition: NWProtocolWebSocket.definition) as? NWProtocolWebSocket.Metadata
        switch metadata?.opcode {
        case .binary?:
            run(viewer.received(connection: number, frame: .binary(bytes: content ?? Data()), nowMs: Self.now()))
        // Text (the relay's `pong` to the viewer's `ping`) and a pong only
        // say the link lives.
        case .text?, .pong?:
            run(viewer.received(connection: number, frame: .text(text: ""), nowMs: Self.now()))
        // A ping Network.framework answered: not fed in, so a peer that
        // only pings never keeps a join that's going nowhere waiting.
        case .ping?:
            break
        case .close?:
            ended(number)
            return
        case .cont?:
            break
        // No WebSocket message at all, nothing in it: the stream ended
        // under it.
        case nil:
            if content == nil {
                ended(number)
                return
            }
        @unknown default:
            break
        }
        if !done, self.connection === connection { receive(on: connection, number) }
    }

    /// Connection `number` ended or failed to open: the viewer hears it
    /// once, if it is still the current one.
    private func ended(_ number: UInt64) {
        guard !done, current == number, let viewer else { return }
        drop()
        tracker.dropped()
        run(viewer.closed(connection: number, nowMs: Self.now()))
    }

    private func send(_ content: Data, _ opcode: NWProtocolWebSocket.Opcode) {
        // With none open, dropped, as the viewer says.
        guard let connection else { return }
        let metadata = NWProtocolWebSocket.Metadata(opcode: opcode)
        let context = NWConnection.ContentContext(identifier: opcode == .binary ? "bin" : "text", metadata: [metadata])
        // A failed send fails the connection, which its state says.
        connection.send(content: content, contentContext: context, isComplete: true, completion: .contentProcessed { _ in })
    }

    /// Drops the current connection: anything more from it is ignored.
    private func drop() {
        current = nil
        guard let connection else { return }
        self.connection = nil
        connection.stateUpdateHandler = nil
        connection.cancel()
    }

    private func halt() {
        guard !done else { return }
        done = true
        drop()
        wake?.cancel()
        wake = nil
        viewer = nil
        session = nil
    }

    /// The tracker's status, if it differs from what was last published.
    private func publishIfChanged() {
        if tracker.status != status { publish() }
    }

    private func publish() {
        let status = tracker.status
        shown.withLock { $0 = status }
        // In order: the main queue runs what it is given first first.
        if let onStatus {
            DispatchQueue.main.async { MainActor.assumeIsolated { onStatus(status) } }
        }
    }

    // MARK: Off the queue

    /// Runs `work`, which blocks, on a thread of its own, then `then` with
    /// what it returned, back on the queue. `then` checks whether it's still
    /// wanted: the connection may have stopped meanwhile. A call can't be
    /// cancelled: it runs until the registry answers or times out.
    private nonisolated func offQueue<T: Sendable>(
        _ queue: DispatchSerialQueue,
        _ work: @escaping @Sendable () -> T,
        then: @escaping @Sendable (isolated RelayConnection, T) -> Void
    ) {
        let thread = Thread {
            let result = work()
            queue.async { self.assumeIsolated { then($0, result) } }
        }
        // Rust's decoding and decompression of a 4 MiB batch, with room.
        thread.stackSize = 8 << 20
        thread.start()
    }

    /// Whether `url` may carry a ticket: `wss` (TLS) always, and plain
    /// `ws` only in a DEBUG build to this machine (true for TLS, false for
    /// plain TCP, nil when refused).
    public static func secure(_ url: URL) -> Bool? {
        switch url.scheme?.lowercased() {
        case "wss": return true
        case "ws":
            #if DEBUG
            if let host = url.host(percentEncoded: false)?.lowercased(), ["127.0.0.1", "::1", "[::1]", "localhost"].contains(host) { return false }
            #endif
            return nil
        default: return nil
        }
    }

    /// A monotonic clock in milliseconds, counting while the phone sleeps:
    /// the viewer's `nowMs`.
    static func now() -> UInt64 {
        clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000
    }

    /// What a failed registry call says: the core's words, never a ticket.
    static func detail(_ error: any Error) -> String {
        switch error as? KrowkError {
        case .InvalidInput(let detail)?, .Internal(let detail)?, .Refused(_, let detail)?, .Network(let detail)?, .Decode(let detail)?:
            detail
        case nil:
            String(describing: error)
        }
    }

    /// The relay a DEBUG build names in `KROWK_RELAY_URL` (the fake
    /// fleet's), passed to `Account.openSession` as its override; nil in a
    /// release build, where Rust picks the relay for the registry.
    public static func relayOverride(environment: [String: String] = ProcessInfo.processInfo.environment) -> String? {
        #if DEBUG
        if let relay = environment["KROWK_RELAY_URL"], !relay.isEmpty { return relay }
        #endif
        return nil
    }
}

/// The viewer's updates for the screen, in order, until the main thread
/// takes them, once per display frame (ticket 89 draws them).
public final class UpdateMailbox: Sendable {
    private let held = OSAllocatedUnfairLock<[ViewerUpdate]>(initialState: [])

    public init() {}

    func post(_ updates: [ViewerUpdate]) {
        guard !updates.isEmpty else { return }
        held.withLock { $0.append(contentsOf: updates) }
    }

    /// Everything posted since the last drain, oldest first.
    public func drain() -> [ViewerUpdate] {
        held.withLock { held in
            defer { held = [] }
            return held
        }
    }
}
