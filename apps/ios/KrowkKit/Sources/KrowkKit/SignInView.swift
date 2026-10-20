import SwiftUI
import UIKit

/// Onboarding's first screen (R-IOS-9): "Sign in to krowk" and one button,
/// which opens the registry's approval page. SwiftUI, as every cold screen
/// is. Leaving the screen cancels the sign-in.
public struct SignInView: View {
    let model: SignInModel

    public init(model: SignInModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Sign in to krowk")
                .font(.largeTitle.bold())
                .accessibilityAddTraits(.isHeader)
                .accessibilityIdentifier("sign-in-title")
            content
            Spacer()
        }
        .padding(24)
        .frame(maxWidth: .infinity, alignment: .leading)
        .onDisappear { model.cancel() }
    }

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .ready:
            Text("Approve this iPhone in the browser, where you're signed in to krowk. No password goes into the app.")
                .foregroundStyle(.secondary)
            Button("Sign in") { model.start() }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .accessibilityIdentifier("sign-in")
        case .starting:
            HStack(spacing: 8) {
                ProgressView()
                Text("Opening the sign-in page…").foregroundStyle(.secondary)
            }
            .accessibilityIdentifier("sign-in-starting")
        case .waiting(let code, let sheetOpen):
            Text("Waiting for approval…")
                .font(.headline)
                .accessibilityIdentifier("sign-in-waiting")
            Text("Check the page shows the code \(code).")
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("sign-in-code")
            HStack(spacing: 12) {
                if !sheetOpen {
                    Button("Open the page again") { model.openPageAgain() }
                        .buttonStyle(.borderedProminent)
                        .accessibilityIdentifier("sign-in-reopen")
                }
                Button("Cancel", role: .cancel) { model.cancel() }
                    .buttonStyle(.bordered)
                    .accessibilityIdentifier("sign-in-cancel")
            }
        case .signedIn(let workspace):
            SignedInView(workspace: workspace)
        case .failed(let failure):
            Text(failure.sentence)
                .accessibilityIdentifier("sign-in-failed")
            Button("Start again") { model.start() }
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("sign-in-start-again")
        }
    }
}

/// Where a signed-in phone goes until pairing exists (ticket 84): "Signed
/// in to <workspace name>".
public struct SignedInView: View {
    let workspace: SignedInWorkspace

    public init(workspace: SignedInWorkspace) {
        self.workspace = workspace
    }

    public var body: some View {
        Text("Signed in to \(workspace.displayName)")
            .font(.headline)
            .accessibilityIdentifier("signed-in")
    }
}

/// The two screens as UIKit view controllers, for the UIKit shell.
public enum SignInScreens {
    @MainActor
    public static func signIn(_ model: SignInModel) -> UIViewController {
        UIHostingController(rootView: SignInView(model: model))
    }

    @MainActor
    public static func signedIn(_ workspace: SignedInWorkspace) -> UIViewController {
        UIHostingController(rootView: SignedInScreen(workspace: workspace))
    }
}

/// The placeholder after sign-in, as a whole screen.
struct SignedInScreen: View {
    let workspace: SignedInWorkspace

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("krowk")
                .font(.largeTitle.bold())
                .accessibilityAddTraits(.isHeader)
            SignedInView(workspace: workspace)
            Spacer()
        }
        .padding(24)
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
