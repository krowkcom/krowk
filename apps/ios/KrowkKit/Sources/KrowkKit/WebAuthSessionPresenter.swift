import AuthenticationServices
import UIKit

/// The approval page in an `ASWebAuthenticationSession` sheet (R-IOS-9).
/// Not ephemeral: the browser's existing registry login is used, so a
/// signed-in person only taps approve. The registry's page never comes back
/// to the app, so the callback scheme never fires; the poll finishes the
/// sign-in and closes the sheet with `dismiss`.
@MainActor
public final class WebAuthSessionPresenter: NSObject, WebAuthPresenter, ASWebAuthenticationPresentationContextProviding {
    private let window: @MainActor () -> UIWindow?
    private var session: ASWebAuthenticationSession?
    /// Which sheet is the current one: a closed one's handler is dropped.
    private var shown = 0

    /// - Parameter window: the scene's key window, which the sheet is shown
    ///   over.
    public init(window: @escaping @MainActor () -> UIWindow?) {
        self.window = window
    }

    public func present(_ page: URL, onClose: @escaping @MainActor () -> Void) {
        dismiss()
        shown &+= 1
        let shown = shown
        // Called when the person closes the sheet, or after `cancel`.
        let session = ASWebAuthenticationSession(url: page, callback: .customScheme("krowk")) { [weak self] _, _ in
            Task { @MainActor in
                guard let self, self.shown == shown, self.session != nil else { return }
                self.session = nil
                onClose()
            }
        }
        session.prefersEphemeralWebBrowserSession = false
        session.presentationContextProvider = self
        self.session = session
        if !session.start() {
            self.session = nil
            onClose()
        }
    }

    public func dismiss() {
        shown &+= 1
        let session = session
        self.session = nil
        session?.cancel()
    }

    public func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        if let window = window() { return window }
        // The window went away: the key window of a scene still connected.
        // An app showing a sheet has one.
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        if let key = scenes.lazy.compactMap(\.keyWindow).first { return key }
        return UIWindow(windowScene: scenes[0])
    }
}
