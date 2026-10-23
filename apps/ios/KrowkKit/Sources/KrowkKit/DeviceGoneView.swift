import Observation
import SwiftUI
import UIKit

/// The screen a phone no longer on the person's devices shows in place of
/// everything else (R-E2E-3, ticket 117): what happened, that it has
/// stopped syncing, and one action, "Reset this iPhone", which forgets its
/// keys (`PhoneReset`) and goes back to sign-in. Nothing is reset without
/// that tap: a registry that lied about a removal costs the person a new
/// pairing, never a key they didn't choose to drop.
@MainActor
@Observable
public final class DeviceGoneModel {
    public let gone: DeviceGone
    /// The reset under way.
    public private(set) var resetting = false
    /// Why the last reset failed; nil before one has, and once one
    /// succeeds.
    public private(set) var failure: String?

    @ObservationIgnored private let reset: PhoneReset
    @ObservationIgnored private let onReset: @MainActor () -> Void

    /// - Parameters:
    ///   - onReset: the keys are gone: the app shows sign-in.
    public init(gone: DeviceGone, reset: PhoneReset, onReset: @escaping @MainActor () -> Void) {
        self.gone = gone
        self.reset = reset
        self.onReset = onReset
    }

    /// "Reset this iPhone". Once at a time; a failure says so and may be
    /// tried again.
    public func resetPhone() {
        guard !resetting else { return }
        resetting = true
        failure = nil
        let reset = reset
        Task {
            do {
                try await reset.run()
                resetting = false
                onReset()
            } catch {
                resetting = false
                failure = "The keys couldn't be forgotten: \(error). Unlock this iPhone and try again."
            }
        }
    }
}

public struct DeviceGoneView: View {
    let model: DeviceGoneModel

    public init(model: DeviceGoneModel) {
        self.model = model
    }

    public var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                // Never markdown: the core's words.
                Text(verbatim: model.gone.title)
                    .font(.title.bold())
                    .accessibilityAddTraits(.isHeader)
                    .accessibilityIdentifier("removed-title")
                Text(verbatim: model.gone.explanation)
                    .accessibilityIdentifier("removed-explanation")
                Text(verbatim: model.gone.detail)
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("removed-detail")
                if let failure = model.failure {
                    Text(verbatim: failure)
                        .foregroundStyle(.red)
                        .accessibilityIdentifier("removed-failed")
                }
                Button(role: .destructive) {
                    model.resetPhone()
                } label: {
                    HStack(spacing: 8) {
                        if model.resetting { ProgressView() }
                        Text("Reset this iPhone")
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .disabled(model.resetting)
                .accessibilityIdentifier("removed-reset")
            }
            .padding(24)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
    }
}

public enum DeviceGoneScreens {
    @MainActor
    public static func gone(_ model: DeviceGoneModel) -> UIViewController {
        UIHostingController(rootView: DeviceGoneView(model: model))
    }
}
