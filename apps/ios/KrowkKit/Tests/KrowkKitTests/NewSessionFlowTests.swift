// R-HOST-3, R-HOST-4, R-IOS-7, R-INST-6, R-LAG-8, R-HAND-4: starting a
// session on a host over a stubbed host source: the host rows (online with
// its version, offline greyed and unchoosable), the directories with home
// as `~`, the models under each instance's limit (one not ready greyed with
// its fix), and the optimistic row in Sessions, replaced by the session or
// by the host's words. No fleet: the hosted NewSessionTests cover the real
// hosts and starts.
import Foundation
import Synchronization
import Testing
import UIKit
@testable import KrowkKit

struct StubInstance: AnnouncedInstance {
    var name = "anthropic"
    var ready = true
    var fix: String?
    var models = ["claude-sonnet-4-6"]
    var limitState: String?
    var limitUsedPercent: Double?
    var limitResetsAtMs: Int64?
}

struct StubHost: AnnouncedHost {
    var device = "a1"
    var name = "desktop"
    var online = true
    var lastSeenMs: UInt64 = 0
    var version = "0.31.0"
    var directories = ["/home/ann/code/app"]
    var announcedInstances: [any AnnouncedInstance] = [StubInstance()]
}

/// Hosts and starts that answer what they're set to, noting where each
/// start ran; a start is held until `release` when `gated`.
final class StubHosts: HostSource {
    private let listing: Mutex<HostListing?>
    private let outcome: Mutex<StartOutcome>
    private let starts = Mutex<[(StartRequest, Bool)]>([])
    private let gate: DispatchSemaphore?

    init(_ hosts: [any AnnouncedHost], outcome: StartOutcome = .started(sessionID: "s1"), gated: Bool = false) {
        listing = Mutex(HostListing(hosts: hosts, unreadable: 0))
        self.outcome = Mutex(outcome)
        gate = gated ? DispatchSemaphore(value: 0) : nil
    }

    func fail() { listing.withLock { $0 = nil } }
    func release() { gate?.signal() }
    var started: [(StartRequest, Bool)] { starts.withLock { $0 } }

    struct Down: Error {}

    func hosts() throws -> HostListing {
        guard let l = listing.withLock({ $0 }) else { throw Down() }
        return l
    }

    func start(_ request: StartRequest) -> StartOutcome {
        starts.withLock { $0.append((request, Thread.isMainThread)) }
        gate?.wait()
        return outcome.withLock { $0 }
    }

    func newCommandID() -> String { UUID().uuidString.lowercased() }
}

@MainActor
@Suite("R-HOST-3, R-IOS-7: the host step")
struct HostStepTests {
    let now = Date(timeIntervalSince1970: 1_790_000_000)

    @Test("R-HOST-3: an online host shows its name and krowk version, and can be chosen")
    func online() {
        let row = HostRowFormatter(locale: Locale(identifier: "en_US")).row(StubHost(), trusted: true, now: now)
        #expect(row.name == "desktop")
        #expect(row.detail == "krowk 0.31.0")
        #expect(row.choosable)
    }

    @Test("R-IOS-7: an offline host reads last seen 12m ago and can't be chosen")
    func offline() {
        let seen = UInt64(now.addingTimeInterval(-12 * 60).timeIntervalSince1970 * 1000)
        let row = HostRowFormatter(locale: Locale(identifier: "en_US")).row(StubHost(online: false, lastSeenMs: seen), trusted: true, now: now)
        #expect(row.detail == "last seen 12m ago", "\(row.detail)")
        #expect(!row.choosable)
    }

    @Test("R-IOS-7: after a failed read the rows stay, none online, and the step says so")
    func failedRead() async {
        let source = StubHosts([StubHost()])
        let model = HostListModel(source: source)
        await model.refresh()
        #expect(model.rows.map(\.choosable) == [true])
        source.fail()
        await model.refresh()
        #expect(model.failed)
        #expect(model.rows.map(\.name) == ["desktop"])
        #expect(model.rows.map(\.choosable) == [false])
        let footer = HostStepViewController.sections(model).first?.footer ?? ""
        #expect(footer.contains("couldn't be read"), "\(footer)")
    }

    @Test("R-IOS-7: the host step's offline row is greyed and not selectable")
    func offlineRowNotSelectable() async {
        let source = StubHosts([StubHost(device: "a1", name: "desktop"), StubHost(device: "b2", name: "laptop", online: false)])
        let screen = HostStepViewController(model: HostListModel(source: source), source: source, onStart: { _ in })
        screen.loadViewIfNeeded()
        await screen.model.refresh()
        // The rows follow the model a main-actor turn later.
        let deadline = Date.now.addingTimeInterval(5)
        while screen.choosable("host-laptop") == nil && Date.now < deadline { await Task.yield() }
        #expect(screen.choosable("host-desktop") == true)
        #expect(screen.choosable("host-laptop") == false)
    }
}

@MainActor
@Suite("R-HOST-4, R-INST-6: the directory and model steps")
struct DirectoryAndModelTests {
    @Test("R-HOST-4: home is shown as ~")
    func tilde() {
        #expect(NewSessionText.homeAsTilde("/home/ann/code/app") == "~/code/app")
        #expect(NewSessionText.homeAsTilde("/Users/ann") == "~")
        #expect(NewSessionText.homeAsTilde("/root/x") == "~/x")
        #expect(NewSessionText.homeAsTilde("/srv/app") == "/srv/app")
        #expect(NewSessionText.homeAsTilde("/home/") == "/home/")
    }

    @Test("R-INST-6: an instance's limit reads allowed, warning: 82% used, or limited until a time")
    func limits() {
        let utc = TimeZone(identifier: "UTC")!
        let en = Locale(identifier: "en_GB")
        #expect(NewSessionText.limit(StubInstance()) == nil)
        #expect(NewSessionText.limit(StubInstance(limitState: "allowed")) == "allowed")
        #expect(NewSessionText.limit(StubInstance(limitState: "warning", limitUsedPercent: 82.4)) == "warning: 82% used")
        let resets = Int64(Date(timeIntervalSince1970: 1_790_000_000).timeIntervalSince1970 * 1000)
        let line = NewSessionText.limit(StubInstance(limitState: "limited", limitResetsAtMs: resets), timeZone: utc, locale: en)
        #expect(line?.hasPrefix("limited until ") == true, "\(String(describing: line))")
    }

    @Test("R-HOST-4, R-INST-6: ready models are listed under the instance and its limit; one not ready is greyed with its fix")
    func models() {
        let sections = ModelStepViewController.sections([
            StubInstance(models: ["a", "b"], limitState: "warning", limitUsedPercent: 82),
            StubInstance(name: "openai", ready: false, fix: "set OPENAI_API_KEY", models: []),
        ])
        #expect(sections.map(\.header) == ["anthropic · warning: 82% used", "openai"])
        #expect(sections[0].choices.map(\.title) == ["a", "b"])
        #expect(sections[0].choices.map(\.enabled) == [true, true])
        #expect(sections[1].choices.map(\.detail) == ["set OPENAI_API_KEY"])
        #expect(sections[1].choices.map(\.enabled) == [false])
    }

    @Test("R-HOST-4: Start sends the place, the model and the words under a fresh id, once however many taps reach it")
    func start() {
        var sent: [StartRequest] = []
        let draft = NewSessionFlow.Draft(device: "a1", hostName: "desktop", cwd: "/home/ann/code/app", instance: "anthropic", model: "claude-sonnet-4-6")
        let step = PromptStepViewController(draft: draft, text: "fix the tests", source: StubHosts([]), onStart: { sent.append($0) })
        step.loadViewIfNeeded()
        step.start()
        step.start()
        #expect(sent.count == 1)
        #expect(sent.map(\.prompt) == ["fix the tests"])
        #expect(sent[0].cwd == "/home/ann/code/app" && sent[0].instance == "anthropic" && sent[0].model == "claude-sonnet-4-6")
        let again = PromptStepViewController(draft: draft, text: "fix the tests", source: StubHosts([]), onStart: { sent.append($0) })
        again.loadViewIfNeeded()
        again.start()
        #expect(sent.count == 2)
        #expect(sent[0].id != sent[1].id, "each Start is a new command")
    }
}

@MainActor
@Suite("R-LAG-8, R-HOST-4: the optimistic row in Sessions")
struct StartingRowTests {
    static let request = StartRequest(id: "c1", device: "a1", hostName: "desktop", cwd: "/home/ann/code/app", instance: "anthropic", model: "m", prompt: "hi")

    @Test("R-LAG-8: a start shows at once as starting on <host>…, runs off the main thread, and gives way to its session")
    func started() async throws {
        let source = StubHosts([], outcome: .started(sessionID: "s9"), gated: true)
        let starting = StartingSessions(source: source)
        var opened: [String] = []
        starting.started = { session, _ in opened.append(session) }
        starting.start(Self.request)
        #expect(starting.rows.map(\.state) == [.starting])
        let list = SessionListViewController(model: SessionListModel(source: StubSource(.success(SessionListing(sessions: [], unreadable: 0)))), newSessions: NewSessions(source: source, starting: starting) { _, _ in UIViewController() })
        #expect(list.screen().starting.map(\.id) == ["start:c1"])
        source.release()
        let deadline = Date.now.addingTimeInterval(5)
        while opened.isEmpty && Date.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        #expect(opened == ["s9"])
        #expect(starting.rows.map(\.state) == [.started("s9")], "kept until Sessions lists it")
        starting.listed("s9")
        #expect(starting.rows.isEmpty)
        #expect(source.started.map(\.1) == [false], "the start ran off the main thread")
    }

    @Test("R-HOST-4: a refusal turns the row into the host's words; Edit and retry keeps it until the edit is sent, as a new command that replaces it")
    func refused() async throws {
        let source = StubHosts([], outcome: .refused(code: "model_not_ready", message: "openai isn't ready — set OPENAI_API_KEY"))
        let starting = StartingSessions(source: source)
        starting.start(Self.request)
        let deadline = Date.now.addingTimeInterval(5)
        while starting.rows.first?.state == .starting && Date.now < deadline { try await Task.sleep(for: .milliseconds(10)) }
        #expect(starting.rows.map(\.state) == [.refused("openai isn't ready — set OPENAI_API_KEY")])
        #expect(starting.refused("start:c1") == Self.request)
        #expect(starting.rows.count == 1, "kept until the edit is sent")
        var again = Self.request
        again.id = "c2"
        starting.start(again, replacing: "start:c1")
        #expect(starting.rows.map(\.id) == ["start:c2"])
    }
}
