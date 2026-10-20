import Foundation
import Observation
import Synchronization

/// A browser sign-in, as the screen needs it (R-IOS-9). KrowkKit does not
/// import the Rust bindings: KrowkAccount implements this over krowk-ffi's
/// `SignIn`, which owns the polling policy, and the tests stub it.
public protocol SignInService: Sendable {
    /// The registry's `/v1` URL: it decides whether an http page may open.
    var api: String { get }
    /// Opens a sign-in on the registry, named `deviceName`. Blocking (a
    /// registry call): never on the main actor.
    func start(deviceName: String) throws(SignInFailure) -> any SignInAttempt
}

/// One sign-in under way.
public protocol SignInAttempt: Sendable {
    /// The code the approval page shows, like `K7QF-9M3X`.
    var code: String { get }
    /// The approval page, unchecked: the model checks it before opening it.
    var verificationURL: String { get }
    /// Blocks until the sign-in is approved (the key is in the Keychain by
    /// then), denied, lapses or is cancelled. Never on the main actor.
    func wait() throws(SignInFailure) -> SignedInWorkspace
    /// Ends `wait` within a moment, and any later one at once.
    func cancel()
}

/// Who the phone signed in as. Never the key: that stays in Rust and the
/// Keychain.
public struct SignedInWorkspace: Sendable, Equatable {
    public let workspace: String
    public let name: String

    public init(workspace: String, name: String) {
        self.workspace = workspace
        self.name = name
    }

    /// The name to show: the workspace's own, else its id.
    public var displayName: String { name.isEmpty ? workspace : name }
}

/// Why a sign-in ended without a key, as the screen tells it.
public enum SignInFailure: Error, Sendable, Equatable {
    /// `authorization_denied`: the person said no in the browser.
    case denied
    /// `authorization_expired`: fifteen minutes passed unapproved.
    case expired
    /// `credentials_unwritable`: the Keychain refused the key. The detail
    /// (its `OSStatus`) is for the log, not the screen.
    case keychainRefused(detail: String)
    /// The registry named a page the app won't open.
    case unsafePage
    /// The registry could not be reached.
    case network(detail: String)
    /// `cancelled`: this app ended it; never shown.
    case cancelled
    /// Anything else, in the core's words.
    case other(detail: String)

    /// One sentence for the screen.
    public var sentence: String {
        switch self {
        case .denied: "You said no in the browser. Start again to sign in."
        case .expired: "The sign-in page expired after 15 minutes. Start again."
        case .keychainRefused: "This iPhone couldn't keep your sign-in safely."
        case .unsafePage: "krowk's registry named a sign-in page this app won't open, so it wasn't opened."
        case .network: "krowk's registry couldn't be reached. Check the connection and start again."
        case .cancelled: "Sign-in was cancelled."
        case .other(let detail): "Sign-in didn't finish: \(detail)."
        }
    }
}

/// What opens the approval page: `ASWebAuthenticationSession` in the app
/// (`WebAuthSessionPresenter`), a recorder in the tests, which play the
/// browser themselves.
@MainActor
public protocol WebAuthPresenter: AnyObject {
    /// Shows `page`. `onClose` is called once, when the sheet closes
    /// without this app closing it (the person closed it, or it could not
    /// open); not after `dismiss`.
    func present(_ page: URL, onClose: @escaping @MainActor () -> Void)
    /// Closes the sheet, if it is open.
    func dismiss()
}

/// The onboarding screen's state (R-IOS-9): start a sign-in off the main
/// actor, open its page, wait for the approval the poll reports, and say
/// how it ended. The poll, not the sheet, finishes a sign-in: closing the
/// sheet leaves it waiting, with the page one tap away.
@MainActor
@Observable
public final class SignInModel {
    public enum Phase: Equatable, Sendable {
        /// "Sign in to krowk", one button.
        case ready
        /// Asking the registry for a sign-in.
        case starting
        /// Waiting for the approval; `sheetOpen` says whether the page is up.
        case waiting(code: String, sheetOpen: Bool)
        /// Done: the key is in the Keychain.
        case signedIn(SignedInWorkspace)
        /// Ended without a key: one sentence and "Start again".
        case failed(SignInFailure)
    }

    public private(set) var phase: Phase = .ready

    @ObservationIgnored private let service: any SignInService
    @ObservationIgnored private let presenter: any WebAuthPresenter
    @ObservationIgnored private let deviceName: String
    @ObservationIgnored private let onSignedIn: @MainActor (SignedInWorkspace) -> Void
    @ObservationIgnored private let log: @Sendable (String) -> Void
    /// The attempt under way, outside the actor so `deinit` can cancel it.
    @ObservationIgnored private let current = Current()
    /// The page of the attempt under way, checked.
    @ObservationIgnored private var page: URL?
    /// Bumped by every start and cancel: an answer for an older run is dropped.
    @ObservationIgnored private var run = 0

    /// - Parameters:
    ///   - deviceName: the name the approval page shows, as
    ///     `SignInRules.deviceName` cleaned it.
    ///   - onSignedIn: moves the app on, once the key is kept.
    ///   - log: where a failure's detail goes (an `OSStatus`, say), never
    ///     the screen.
    public init(
        service: any SignInService,
        presenter: any WebAuthPresenter,
        deviceName: String,
        log: @escaping @Sendable (String) -> Void = { _ in },
        onSignedIn: @escaping @MainActor (SignedInWorkspace) -> Void = { _ in }
    ) {
        self.service = service
        self.presenter = presenter
        self.deviceName = SignInRules.deviceName(deviceName)
        self.log = log
        self.onSignedIn = onSignedIn
    }

    deinit {
        current.take()?.cancel()
    }

    /// "Sign in to krowk" and "Start again": opens a sign-in and its page.
    public func start() {
        switch phase {
        case .starting, .waiting, .signedIn: return
        case .ready, .failed: break
        }
        run &+= 1
        let run = run
        phase = .starting
        let service = service
        let name = deviceName
        // The calls block (ureq): a detached task, never the main actor.
        Task.detached(priority: .userInitiated) { [weak self] in
            let attempt: any SignInAttempt
            do throws(SignInFailure) {
                attempt = try service.start(deviceName: name)
            } catch {
                await MainActor.run { self?.end(run, .failure(error)) }
                return
            }
            let go = await MainActor.run { self?.opened(attempt, run) ?? false }
            guard go else {
                attempt.cancel()
                return
            }
            let result: Result<SignedInWorkspace, SignInFailure>
            do throws(SignInFailure) {
                result = .success(try attempt.wait())
            } catch {
                result = .failure(error)
            }
            await MainActor.run { self?.end(run, result) }
        }
    }

    /// "Open the page again", after the person closed the sheet.
    public func openPageAgain() {
        guard case .waiting(let code, false) = phase, let page else { return }
        phase = .waiting(code: code, sheetOpen: true)
        present(page, run)
    }

    /// "Cancel", or the screen going away: ends the poll and the sheet.
    public func cancel() {
        run &+= 1
        current.take()?.cancel()
        page = nil
        presenter.dismiss()
        switch phase {
        case .starting, .waiting: phase = .ready
        case .ready, .signedIn, .failed: break
        }
    }

    /// The sign-in is open: check its page and show it, or refuse it.
    private func opened(_ attempt: any SignInAttempt, _ run: Int) -> Bool {
        guard run == self.run, phase == .starting else { return false }
        guard let page = SignInRules.browsablePage(attempt.verificationURL, api: service.api) else {
            log("sign-in: refused to open the page \(attempt.verificationURL) for the registry \(service.api)")
            phase = .failed(.unsafePage)
            return false
        }
        current.set(attempt)
        self.page = page
        phase = .waiting(code: attempt.code, sheetOpen: true)
        present(page, run)
        return true
    }

    private func present(_ page: URL, _ run: Int) {
        presenter.present(page) { [weak self] in
            guard let self, run == self.run, case .waiting(let code, true) = self.phase else { return }
            self.phase = .waiting(code: code, sheetOpen: false)
        }
    }

    private func end(_ run: Int, _ result: Result<SignedInWorkspace, SignInFailure>) {
        guard run == self.run else { return }
        _ = current.take()
        page = nil
        switch result {
        case .success(let workspace):
            phase = .signedIn(workspace)
            presenter.dismiss()
            onSignedIn(workspace)
        case .failure(.cancelled):
            presenter.dismiss()
            phase = .ready
        case .failure(let failure):
            switch failure {
            case .keychainRefused(let detail), .network(let detail), .other(let detail): log("sign-in: \(detail)")
            default: break
            }
            presenter.dismiss()
            phase = .failed(failure)
        }
    }
}

/// The attempt under way, behind a lock: set and taken on the main actor,
/// taken once more by `deinit`, which is not.
private final class Current: Sendable {
    private let attempt = Mutex<(any SignInAttempt)?>(nil)

    func set(_ new: any SignInAttempt) {
        attempt.withLock { $0 = new }
    }

    func take() -> (any SignInAttempt)? {
        attempt.withLock { a in
            defer { a = nil }
            return a
        }
    }
}
