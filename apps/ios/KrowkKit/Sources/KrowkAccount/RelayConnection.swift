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
/// once per display frame (`takeFrame`), waking the screen's paused
/// display link when they arrive with none waiting. Nothing here logs: a
/// relay ticket never leaves the call that dials with it.
///
/// When it connects and tries again is its `ConnectionSupervisor`'s
/// (krowk-ffi, ticket 86, R-IOS-2, R-OFF-2): a failed open or join waits
/// on a jittered backoff from 250 ms to 30 s; in the background a live
/// connection is kept but a lost one waits for the foreground; the scene's
/// foreground, its activation, a network path that became usable
/// (`NWPathMonitor`, on this queue) and pull-to-refresh wake it — joining
/// at once, replacing a silent or long-backgrounded connection, or probing
/// a live one with a `ping`. Every join resumes at the core's cursor.
public actor RelayConnection: LiveSession {
    /// The relay's largest message, its header and a 4 MiB batch
    /// (`krowk_harness::relay`'s `max_message`): anything larger fails the
    /// connection, and the viewer joins again.
    static let maxMessage = 28 + (4 << 20)

    public nonisolated let mailbox = UpdateMailbox()

    private let queue: DispatchSerialQueue
    public nonisolated var unownedExecutor: UnownedSerialExecutor { queue.asUnownedSerialExecutor() }

    /// Opens the session from the registry: blocking.
    private let open: @Sendable () throws -> OpenedSession
    private var session: OpenedSession?
    private var viewer: Viewer?
    /// The connection the viewer dialed last, and its number, until it is
    /// dropped or ends.
    private var connection: NWConnection?
    private var current: UInt64?
    /// When the viewer next wants `tick`.
    private var wakeTimer: DispatchSourceTimer?
    /// The `NeedTicket` acted on: a later one replaces it, and a ticket
    /// fetched for an earlier one is dropped.
    private var ticketAsk: UInt64 = 0
    private var started = false
    /// Gaps seen: a later one restarts the wait for its catch-up.
    private var gaps = 0
    /// How long `catching up…` waits for a catch-up that may bring nothing.
    static let gapWait: DispatchTimeInterval = .seconds(5)
    /// Stopped, or the viewer ended: nothing more is called.
    private var done = false
    /// When to connect and try again (ticket 86): asked while the session
    /// is opened, then handed to the viewer. Made on the queue the first
    /// time it's needed, never on the main thread, where this is made.
    private var supervisor: ConnectionSupervisor {
        if let madeSupervisor { return madeSupervisor }
        let made = ConnectionSupervisor(nowMs: now(), seed: seed)
        madeSupervisor = made
        return made
    }
    private var madeSupervisor: ConnectionSupervisor?
    private let seed = UInt64.random(in: .min ... .max)
    /// Opening the session, on a thread of its own, and which open is the
    /// current one: one the background abandoned is dropped when it ends.
    private var opening = false
    private var openAsk: UInt64 = 0
    /// The next try at opening it, after one failed.
    private var openRetry: DispatchWorkItem?
    /// The network path, watched from `start`: a path that becomes usable
    /// wakes the connection.
    private var path: NWPathMonitor?
    private var paths = PathChanges()
    /// Commands sent before the viewer was open, in order: they go to it
    /// as it opens.
    private var early: [(command: LiveCommand, token: UInt64)] = []
    /// Commands handed to the viewer whose `sent` it hasn't shown yet, in
    /// the order handed: the core shows them on a later `tick`, one `sent`
    /// per command in that order, so the oldest token is the next `sent`'s.
    private var unminted: [UInt64] = []
    /// The session's id, once open: what its commands name.
    private var sessionID: String?

    private var tracker = RelayStatusTracker()
    private let shown = OSAllocatedUnfairLock<RelayStatus>(initialState: .connecting)
    private var onStatus: (@MainActor @Sendable (RelayStatus) -> Void)?
    private let counts = OSAllocatedUnfairLock<Int>(initialState: 0)
    /// Where approvals asked and answered live go (ticket 91).
    private nonisolated let approvals: any ApprovalSink

    /// `opening` opens the session to follow (`Account.openSession`), and
    /// blocks; it runs once, on a thread of its own, as `follow` starts.
    public init(opening: @escaping @Sendable () throws -> OpenedSession, approvals: any ApprovalSink = NoApprovals()) {
        self.queue = DispatchSerialQueue(label: "krowk.relay")
        self.open = opening
        self.approvals = approvals
    }

    /// One never stopped (its screen let go of it) still closes its socket.
    deinit {
        connection?.stateUpdateHandler = nil
        connection?.cancel()
        wakeTimer?.cancel()
        path?.cancel()
        // A retry of the open, if one waits, holds this weakly: it finds
        // it gone and does nothing.
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

    public nonisolated func onUpdates(_ wake: @escaping @MainActor @Sendable () -> Void) {
        mailbox.onPost {
            DispatchQueue.main.async { MainActor.assumeIsolated { wake() } }
        }
    }

    @MainActor public func takeFrame() -> [TranscriptUpdate] {
        TranscriptUpdate.from(posted: mailbox.drainAll(), approvals: approvals)
    }

    /// The composer's command, for the viewer (ticket 90): on the queue, as
    /// every other input; never on the main thread. The core mints its id,
    /// shows it sent or queued, resends it after a reconnect until acked,
    /// and the host runs it once; nothing here keeps or resends it. Before
    /// the session is open it waits for the viewer, in order; once stopped,
    /// it never goes, and the screen hears why.
    public nonisolated func send(_ command: LiveCommand, token: UInt64) {
        queue.async { self.assumeIsolated { $0.issue(command, token: token) } }
    }

    /// The scene is coming forward (`willEnterForeground`, R-IOS-2): before
    /// any view appears, the connection is checked — joined at once if it
    /// was lost, replaced after 10 s or more away, probed otherwise.
    public nonisolated func foreground() {
        queue.async { self.assumeIsolated { $0.lifecycle(.foreground) } }
    }

    /// The scene went to the background: the connection is kept while iOS
    /// lets it be, and one lost meanwhile waits for the foreground.
    public nonisolated func background() {
        queue.async { self.assumeIsolated { $0.lifecycle(.background) } }
    }

    /// The scene became active, or the person pulled to refresh: joined at
    /// once if lost, else probed.
    public nonisolated func wake() {
        queue.async { self.assumeIsolated { $0.lifecycle(.wake) } }
    }

    #if DEBUG
    /// Ends the current connection as a network drop would (`forceCancel`):
    /// the viewer hears it closed, and joins again at its cursor. For the
    /// tests.
    public nonisolated func dropConnection() {
        queue.async { self.assumeIsolated { $0.connection?.forceCancel() } }
    }

    /// A network path change, as `NWPathMonitor` reports one after its
    /// first: a usable path wakes the connection. For the tests, which
    /// can't change the simulator's network.
    public nonisolated func pathChanged(satisfied: Bool) {
        queue.async { self.assumeIsolated { $0.pathMoved(satisfied: satisfied) } }
    }

    /// Moves this connection's clock on by `by`, as time passing would, for
    /// the viewer and its supervisor (a long background, without waiting
    /// it out). On the queue, in order with `background` and `foreground`
    /// called around it. For the tests; the timers already armed keep
    /// their time.
    public nonisolated func advanceClock(by: Duration) {
        let ms = UInt64(by.components.seconds) * 1000 + UInt64(by.components.attoseconds / 1_000_000_000_000_000)
        queue.async { self.clockOffset.withLock { $0 += ms } }
    }

    private nonisolated let clockOffset = OSAllocatedUnfairLock<UInt64>(initialState: 0)

    /// The text `ping`s sent (the heartbeat's and the probes') and the
    /// text or pong messages heard back, for the tests.
    public nonisolated var heartbeats: (pings: Int, pongs: Int) { beats.withLock { ($0.pings, $0.pongs) } }
    private nonisolated let beats = OSAllocatedUnfairLock<(pings: Int, pongs: Int)>(initialState: (0, 0))
    #endif

    // MARK: Driving the viewer

    private func start(_ status: (@MainActor @Sendable (RelayStatus) -> Void)?) {
        guard !started, !done else { return }
        started = true
        onStatus = status
        publish()
        watchPath()
        openSession()
    }

    /// Opens the session and its viewer, off the queue: once at the start,
    /// and again after a failure the network may mend.
    private func openSession() {
        guard !done, viewer == nil, !opening else { return }
        opening = true
        openAsk += 1
        let ask = openAsk
        openRetry?.cancel()
        openRetry = nil
        let open = self.open
        let supervisor = self.supervisor
        let now = self.now()
        offQueue(queue) {
            Result { () throws -> (OpenedSession, Viewer) in
                let session = try open()
                return (session, try Viewer.openSupervised(session: session, supervisor: supervisor, nowMs: now))
            }
        } then: { (me: isolated RelayConnection, opened) in
            guard ask == me.openAsk else { return }
            me.opening = false
            me.opened(opened)
        }
    }

    private func opened(_ opened: Result<(OpenedSession, Viewer), any Error>) {
        guard !done else { return }
        switch opened {
        case .success(let (session, viewer)):
            tracker.opened()
            self.session = session
            self.viewer = viewer
            sessionID = session.id()
            let timer = DispatchSource.makeTimerSource(queue: queue)
            timer.setEventHandler { [weak self] in self?.assumeIsolated { $0.tick() } }
            timer.resume()
            wakeTimer = timer
            run(viewer.start(nowMs: now()))
            let waiting = early
            early = []
            for (command, token) in waiting { issue(command, token: token) }
        // The registry unreachable, a rate limit or a 5xx: tried again on
        // the backoff, or at the foreground; the bar says why and when.
        // Commands typed meanwhile wait for the viewer.
        case .failure(let error) where Self.retryable(error):
            let now = self.now()
            if let at = supervisor.lost(nowMs: now) {
                let retry = DispatchWorkItem { [weak self] in self?.assumeIsolated { $0.openSession() } }
                openRetry = retry
                queue.asyncAfter(deadline: .now() + .milliseconds(Int(min(at > now ? at - now : 0, 60_000))), execute: retry)
                tracker.openFailed(Self.detail(error), retryAt: Self.instant(at, now: now))
            } else {
                tracker.openFailed(Self.detail(error), retryAt: .now)
            }
            publishIfChanged()
        case .failure(let error):
            done = true
            // Before the status: the screen applies what the mailbox holds
            // as it hears the end, and these never reached the core.
            unsendEarly()
            tracker.ended(Self.detail(error))
            publishIfChanged()
        }
    }

    /// A failed open the network may mend: the registry unreachable, a
    /// rate limit or a 5xx (`Network`). Anything else (the session gone,
    /// access refused) stops.
    static func retryable(_ error: any Error) -> Bool {
        if case .Network? = error as? KrowkError { true } else { false }
    }

    /// The lifecycle's events, as the scene and the network say them.
    private enum Lifecycle { case foreground, background, wake }

    private func lifecycle(_ event: Lifecycle) {
        let now = self.now()
        guard started, !done else {
            // Not followed yet: only where the app is, for when it is.
            switch event {
            case .background: supervisor.background(nowMs: now)
            case .foreground: _ = supervisor.foreground(nowMs: now, link: .down)
            case .wake: break
            }
            return
        }
        if let viewer {
            switch event {
            case .foreground: run(viewer.foreground(nowMs: now))
            case .background:
                // A ticket asked for and not yet used is dropped (the
                // viewer holds its join): the foreground asks afresh, so a
                // fetch iOS suspended mid-call never holds the return up.
                ticketAsk += 1
                run(viewer.background(nowMs: now))
            case .wake: run(viewer.wake(nowMs: now))
            }
            return
        }
        // Still opening the session, or waiting to try again.
        let link: LinkState = opening ? .joining : .down
        let action: WakeAction
        switch event {
        case .background:
            // Neither retried nor waited on there: an open under way is
            // abandoned (its answer dropped) and one due later is not
            // made; the foreground opens it at once.
            supervisor.background(nowMs: now)
            openRetry?.cancel()
            openRetry = nil
            if opening {
                opening = false
                openAsk += 1
            }
            return
        case .foreground: action = supervisor.foreground(nowMs: now, link: link)
        case .wake: action = supervisor.wake(nowMs: now, link: link)
        }
        if action == .connect { openSession() }
    }

    /// Watches the network path on this queue. Its first callback is the
    /// path as it is, not a change (`PathChanges`).
    private func watchPath() {
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            let satisfied = path.status == .satisfied
            self?.assumeIsolated { $0.pathUpdate(satisfied: satisfied) }
        }
        monitor.start(queue: queue)
        path = monitor
    }

    private func pathUpdate(satisfied: Bool) {
        if paths.changed() { pathMoved(satisfied: satisfied) }
    }

    /// A usable path after a change (Wi-Fi to cellular, a VPN up or
    /// down): the connection woken. An unusable one does nothing: the
    /// socket says so itself, and nothing could be joined meanwhile.
    private func pathMoved(satisfied: Bool) {
        if satisfied { lifecycle(.wake) }
    }

    /// A command of the person's into the viewer: sent now, or queued in
    /// the core. The core shows it `sent` (with its id) on a later frame;
    /// `posting` ties that `sent` to this token.
    private func issue(_ command: LiveCommand, token: UInt64) {
        guard !done else {
            mailbox.post([.unsent(token: token, why: Self.stoppedWhy)])
            return
        }
        guard let viewer, let sessionID else {
            early.append((command, token))
            return
        }
        unminted.append(token)
        run(viewer.command(command: command.command(session: sessionID), nowMs: now()))
    }

    /// A `Show`'s updates for the mailbox: each `sent` preceded by the id
    /// it gives the oldest command still waiting for one, so the screen
    /// knows which bubble the `sent` and its later `acked` are.
    private func posting(_ updates: [ViewerUpdate]) -> [MailboxItem] {
        var items: [MailboxItem] = []
        items.reserveCapacity(updates.count)
        for update in updates {
            if case .sent(let id, _) = update, !unminted.isEmpty {
                items.append(.minted(token: unminted.removeFirst(), id: id))
            }
            items.append(.viewer(update))
        }
        return items
    }

    /// Commands that will never reach the core, or never be shown by it
    /// (the viewer ended): said, so their bubbles don't wait forever.
    private func unsendEarly() {
        let waiting = early.map(\.token) + unminted
        early = []
        unminted = []
        if !waiting.isEmpty { mailbox.post(waiting.map { .unsent(token: $0, why: Self.stoppedWhy) }) }
    }

    /// Why a command never went.
    static let stoppedWhy = "Not sent: the connection to this session stopped. Open the session again to send it."


    private func tick() {
        guard !done, let viewer else { return }
        run(viewer.tick(nowMs: now()))
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
                // The supervisor's backoff: the bar counts down to it.
                let now = self.now()
                if atMs > now { tracker.retry(at: Self.instant(atMs, now: now)) }
            case .dial(let url, let ticket, let connection):
                dial(url: url, ticket: ticket, number: connection)
            case .send(let bytes):
                send(bytes, .binary)
            case .ping:
                #if DEBUG
                beats.withLock { $0.pings += 1 }
                #endif
                send(Data("ping".utf8), .text)
            case .close:
                drop()
                tracker.dropped()
            case .show(let updates):
                for update in updates {
                    switch update {
                    case .host(let present): tracker.host(present)
                    case .note(let text): tracker.note(text)
                    case .gap:
                        tracker.gap()
                        settleGap()
                    case .caughtUp: tracker.caughtUp()
                    case .failed(let detail): failure = detail
                    default: break
                    }
                }
                mailbox.post(posting(updates))
            case .readChunks:
                offQueue(queue) { viewer.readChunks() } then: { me, read in
                    guard !me.done, let viewer = me.viewer else { return }
                    me.run(viewer.chunksRead(read: read, nowMs: me.now()))
                }
            case .end:
                tracker.ended(failure)
                halt()
            }
        }
        publishIfChanged()
        armWake()
    }

    /// A gap's catch-up that brings nothing new (the batch the relay
    /// dropped held only typing) is never announced by the core: after
    /// `gapWait` with no `caughtUp`, the bar reads live again.
    private func settleGap() {
        gaps += 1
        let gap = gaps
        queue.asyncAfter(deadline: .now() + Self.gapWait) { [weak self] in
            self?.assumeIsolated { me in
                guard !me.done, gap == me.gaps else { return }
                me.tracker.caughtUp()
                me.publishIfChanged()
            }
        }
    }

    private func armWake() {
        guard !done, let wakeTimer, let at = viewer?.nextWakeMs() else { return }
        let now = self.now()
        // At least once a minute, whatever it asked: a clock read late
        // never leaves the viewer asleep.
        let wait = Int(min(at > now ? at - now : 0, 60_000))
        // Uptime, not the wall clock: setting the clock back never holds a
        // timer up. Uptime stops while the phone sleeps, and so does the
        // app (the background); the foreground's `foreground` re-arms.
        wakeTimer.schedule(deadline: .now() + .milliseconds(wait), leeway: .milliseconds(4))
    }

    /// `NeedTicket`: at `atMs`, the ticket from the registry, off the
    /// queue; then `ticket` or `ticketFailed`. The ticket goes from that
    /// call to the viewer and is never kept or logged.
    private func askTicket(at atMs: UInt64) {
        ticketAsk += 1
        let ask = ticketAsk
        let now = self.now()
        let wait = Int(min(atMs > now ? atMs - now : 0, 60_000))
        // Uptime, as `armWake`'s; a later `NeedTicket` (a wake's, now)
        // replaces this one.
        queue.asyncAfter(deadline: .now() + .milliseconds(wait)) { [weak self] in
            self?.assumeIsolated { me in
                guard !me.done, ask == me.ticketAsk, let session = me.session else { return }
                me.offQueue(me.queue) { Result { try session.relayTicket() } } then: { me, fetched in
                    guard !me.done, ask == me.ticketAsk, let viewer = me.viewer else { return }
                    switch fetched {
                    case .success(let ticket): me.run(viewer.ticket(ticket: ticket, nowMs: me.now()))
                    case .failure(let error): me.run(viewer.ticketFailed(detail: Self.detail(error), nowMs: me.now()))
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
            run(viewer.received(connection: number, frame: .binary(bytes: content ?? Data()), nowMs: now()))
        // Text (the relay's `pong` to the viewer's `ping`) and a pong only
        // say the link lives.
        case .text?, .pong?:
            #if DEBUG
            beats.withLock { $0.pongs += 1 }
            #endif
            run(viewer.received(connection: number, frame: .text(text: ""), nowMs: now()))
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
        run(viewer.closed(connection: number, nowMs: now()))
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
        wakeTimer?.cancel()
        wakeTimer = nil
        path?.cancel()
        path = nil
        openRetry?.cancel()
        openRetry = nil
        viewer = nil
        session = nil
        unsendEarly()
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
    /// the viewer's `nowMs`. The wall clock set back or forward never
    /// moves it.
    static func now() -> UInt64 {
        clock_gettime_nsec_np(CLOCK_MONOTONIC) / 1_000_000
    }

    /// This connection's `now`: `Self.now()`, moved on in a DEBUG build by
    /// the tests' `advanceClock`.
    private nonisolated func now() -> UInt64 {
        #if DEBUG
        Self.now() + clockOffset.withLock { $0 }
        #else
        Self.now()
        #endif
    }

    /// `at`, a `now()` in milliseconds, as an instant on the continuous
    /// clock (both count while the phone sleeps), for the bar's countdown.
    static func instant(_ at: UInt64, now: UInt64) -> ContinuousClock.Instant {
        ContinuousClock.now.advanced(by: .milliseconds(Int64(at > now ? at - now : 0)))
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
/// takes them, once per display frame, into the transcript
/// (`takeFrame()`). The first post after a drain calls `onPost`'s wake.
public final class UpdateMailbox: Sendable {
    private struct Held: Sendable {
        var updates: [MailboxItem] = []
        var wake: (@Sendable () -> Void)?
    }

    private let held = OSAllocatedUnfairLock<Held>(initialState: Held())

    public init() {}

    func post(_ updates: [MailboxItem]) {
        guard !updates.isEmpty else { return }
        let wake = held.withLock { held -> (@Sendable () -> Void)? in
            let waiting = !held.updates.isEmpty
            held.updates.append(contentsOf: updates)
            return waiting ? nil : held.wake
        }
        // Outside the lock: the wake hops to the main queue.
        wake?()
    }

    /// `wake` is called (on the poster's queue) when updates arrive with
    /// none waiting, and at once if some are.
    public func onPost(_ wake: @escaping @Sendable () -> Void) {
        let waiting = held.withLock { held in
            held.wake = wake
            return !held.updates.isEmpty
        }
        if waiting { wake() }
    }

    /// How many updates wait to be drained.
    public var waiting: Int { held.withLock { $0.updates.count } }

    /// Everything posted since the last drain, oldest first.
    public func drainAll() -> [MailboxItem] {
        held.withLock { held in
            defer { held.updates = [] }
            return held.updates
        }
    }

    /// The viewer's updates posted since the last drain, oldest first (the
    /// commands' ids dropped): for the tests.
    public func drain() -> [ViewerUpdate] {
        drainAll().compactMap { if case .viewer(let update) = $0 { update } else { nil } }
    }
}
