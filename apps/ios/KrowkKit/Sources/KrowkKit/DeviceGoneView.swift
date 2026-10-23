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
    /// "Reset this iPhone?" is up, waiting for the person.
    public var confirming = false
    /// A check asked for with "Check again" under way.
    public private(set) var checking = false
    /// Why the last reset failed, or what the last check again found;
    /// nil before either.
    public private(set) var failure: String?
    /// The list this phone kept names no other device and no recovery kit:
    /// it may hold the only copy of the person's keys, and the
    /// confirmation says what a reset then loses.
    public private(set) var onlyCopy = false

    @ObservationIgnored private let reset: PhoneReset
    @ObservationIgnored private let recheck: @Sendable () -> Standing
    @ObservationIgnored private let onReset: @MainActor () -> Void
    @ObservationIgnored private let onListed: @MainActor () -> Void

    /// - Parameters:
    ///   - recheck: the device list checked again (blocking): a registry
    ///     that misspoke, or a revoke undone, finds the phone listed.
    ///   - onReset: the keys are gone: the app shows sign-in.
    ///   - onListed: a check again found the phone on the list: the app
    ///     opens Sessions again.
    ///   - onlyCopy: whether the list this phone kept names no other
    ///     device and no recovery kit (blocking: the Keychain), read once.
    public init(
        gone: DeviceGone, reset: PhoneReset, recheck: @escaping @Sendable () -> Standing = { .unknown("") },
        onlyCopy: @escaping @Sendable () -> Bool = { false },
        onReset: @escaping @MainActor () -> Void, onListed: @escaping @MainActor () -> Void = {}
    ) {
        self.gone = gone
        self.reset = reset
        self.recheck = recheck
        self.onReset = onReset
        self.onListed = onListed
        Task { [weak self] in
            let only = await blocking { onlyCopy() }
            self?.onlyCopy = only
        }
    }

    /// What the confirmation says.
    public var confirmation: String {
        let lost = onlyCopy
            ? "Your device list as this iPhone last saw it names no other device and no recovery kit: this iPhone may hold the only copy of your keys, and resetting it loses every session for good. "
            : ""
        return lost + "It forgets every key it keeps and signs in as a new device, to pair again. If your device list still shows it, remove it there with krowk devices remove."
    }

    /// "Check again": the phone listed after all goes back to Sessions;
    /// anything else says what was found, and nothing is reset.
    public func checkAgain() {
        guard !checking, !resetting else { return }
        checking = true
        failure = nil
        let recheck = recheck
        Task {
            let found = await blocking { recheck() }
            checking = false
            switch found {
            case .listed: onListed()
            case .gone: failure = "Checked again: this iPhone is still not on your devices."
            case .untrusted(let why), .unknown(let why): failure = "Couldn't check again: \(why)"
            }
        }
    }

    /// "Reset this iPhone", confirmed. Once at a time; a failure says so
    /// and may be tried again.
    public func resetPhone() {
        confirming = false
        guard !resetting, !checking else { return }
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
    @Bindable var model: DeviceGoneModel

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
                    model.confirming = true
                } label: {
                    HStack(spacing: 8) {
                        if model.resetting { ProgressView() }
                        Text("Reset this iPhone")
                    }
                }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .disabled(model.resetting || model.checking)
                .accessibilityIdentifier("removed-reset")
                Button {
                    model.checkAgain()
                } label: {
                    HStack(spacing: 8) {
                        if model.checking { ProgressView() }
                        Text("Check again")
                    }
                }
                .disabled(model.resetting || model.checking)
                .accessibilityIdentifier("removed-recheck")
            }
            .padding(24)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .alert("Reset this iPhone?", isPresented: $model.confirming) {
            Button("Reset", role: .destructive) { model.resetPhone() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(verbatim: model.confirmation)
        }
    }
}

public enum DeviceGoneScreens {
    @MainActor
    public static func gone(_ model: DeviceGoneModel) -> UIViewController {
        UIHostingController(rootView: DeviceGoneView(model: model))
    }
}
