import AVFoundation
import SwiftUI
import UIKit
import VisionKit

/// The pairing screen (R-E2E-3), after sign-in: scan the QR code `krowk
/// devices add` shows, or type its code; then "Pairing with <laptop>",
/// and "This iPhone is on your devices". SwiftUI, as every cold screen is.
/// The screen owns its model, so leaving it (the root replaced) ends the
/// model, and its `deinit` cancels the join and ends the pairing.
public struct PairingView: View {
    @Bindable var model: PairingModel
    let workspace: SignedInWorkspace?
    @FocusState private var codeFocused: Bool

    public init(model: PairingModel, workspace: SignedInWorkspace?) {
        self.model = model
        self.workspace = workspace
    }

    public var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("Add this iPhone")
                    .font(.largeTitle.bold())
                    .accessibilityAddTraits(.isHeader)
                    .accessibilityIdentifier("pairing-title")
                if let workspace {
                    SignedInView(workspace: workspace)
                }
                content
            }
            .padding(24)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .sheet(isPresented: scannerOpen) {
            ScannerSheet(
                onScanned: { model.scanned($0) }, onFailed: { model.scannerFailed() }, onClose: { model.closeScanner() })
        }
    }

    private var scannerOpen: Binding<Bool> {
        // Closing the sheet closes the scanner, but never wipes a notice set
        // as it closed (a camera that wouldn't start).
        Binding(get: { model.scanner == .open }, set: { if !$0, model.scanner == .open { model.closeScanner() } })
    }

    @ViewBuilder
    private var content: some View {
        switch model.phase {
        case .entering:
            entering
        case .finding:
            HStack(spacing: 8) {
                ProgressView()
                Text("Looking for your laptop's pairing…").foregroundStyle(.secondary)
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("pairing-finding")
            cancelButton
        case .pairing(let laptop):
            Text("Pairing with **\(laptop)**")
                .font(.headline)
                .accessibilityIdentifier("pairing-with")
            Text("Your laptop asks whether to add '\(model.deviceName)'. Answer Y there.")
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("pairing-answer")
            HStack(spacing: 8) {
                ProgressView()
                Text("Waiting for your laptop…").foregroundStyle(.secondary)
            }
            .accessibilityElement(children: .combine)
            cancelButton
        case .paired:
            Text("This iPhone is on your devices.")
                .font(.headline)
                .accessibilityIdentifier("pairing-done")
            if let kit = model.recoveryKit {
                // krowk's own sentence, read as markdown for its commands.
                Text(LocalizedStringKey(RecoveryKitReminder.sentence(hasKit: kit)))
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("pairing-kit")
            }
            Button("Continue") { model.finish() }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .accessibilityIdentifier("pairing-continue")
        }
    }

    @ViewBuilder
    private var entering: some View {
        Group {
            if model.scannerSupported {
                Text("On your laptop run `krowk devices add`, then scan its QR code or type the code.")
            } else {
                Text("On your laptop run `krowk devices add`, then type the code it shows.")
            }
        }
        .foregroundStyle(.secondary)
        .accessibilityIdentifier("pairing-intro")
        if let failure = model.failure {
            // Never read as markdown: it can carry the other device's name.
            Text(Self.styled(failure, laptop: model.laptopName))
                .accessibilityIdentifier("pairing-failed")
        }
        if model.scannerSupported {
            Button {
                model.scan()
            } label: {
                Label("Scan the QR code", systemImage: "qrcode.viewfinder")
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .accessibilityIdentifier("pairing-scan")
            switch model.scanner {
            case .refused:
                Text("krowk can't use the camera. Allow it in Settings, or type the code.")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("pairing-camera-refused")
                Button("Open Settings") {
                    if let url = URL(string: UIApplication.openSettingsURLString) {
                        UIApplication.shared.open(url)
                    }
                    model.closeScanner()
                }
                .accessibilityIdentifier("pairing-open-settings")
            case .unavailable:
                Text("The camera isn't available to krowk on this iPhone. Type the code instead.")
                    .foregroundStyle(.secondary)
                    .accessibilityIdentifier("pairing-camera-unavailable")
            case .closed, .open:
                EmptyView()
            }
        }
        TextField("Code, like K7QF-9M3X", text: Binding(get: { model.codeText }, set: { model.enter($0) }))
            .font(.title3.monospaced())
            .textInputAutocapitalization(.characters)
            .autocorrectionDisabled()
            .keyboardType(.asciiCapable)
            .submitLabel(.go)
            .focused($codeFocused)
            .onSubmit { model.submitCode() }
            .padding(12)
            .background(.quaternary, in: RoundedRectangle(cornerRadius: 10))
            .accessibilityLabel("Pairing code")
            .accessibilityIdentifier("pairing-code")
        Button("Pair") {
            codeFocused = false
            model.submitCode()
        }
        .buttonStyle(.bordered)
        .controlSize(.large)
        .disabled(model.codeText.trimmingCharacters(in: .whitespaces).isEmpty)
        .accessibilityIdentifier("pairing-submit")
    }

    /// The failure's sentence as plain text, its back-quoted commands in
    /// monospace. The device's name goes in after the styling, as it is:
    /// nothing in it is read as markdown or as a back quote, so a name
    /// can't become a link or restyle the sentence.
    static func styled(_ failure: PairingFailure, laptop: String?) -> AttributedString {
        let slot = "\u{FFFC}"
        let template = failure.sentence(laptop: laptop.map { _ in slot })
        var out = AttributedString()
        for (i, part) in template.split(separator: "`", omittingEmptySubsequences: false).enumerated() {
            var piece = AttributedString(String(part))
            if i % 2 == 1 { piece.inlinePresentationIntent = .code }
            out += piece
        }
        if let laptop, let at = out.range(of: slot) {
            out.replaceSubrange(at, with: AttributedString(laptop))
        }
        return out
    }

    private var cancelButton: some View {
        Button("Cancel", role: .cancel) { model.cancel() }
            .buttonStyle(.bordered)
            .accessibilityIdentifier("pairing-cancel")
    }
}

/// VisionKit's scanner in a sheet, QR codes only: each payload goes to
/// `onScanned`, which keeps what is krowk's and ignores the rest.
struct ScannerSheet: View {
    let onScanned: @MainActor (String) -> Void
    let onFailed: @MainActor () -> Void
    let onClose: @MainActor () -> Void

    var body: some View {
        NavigationStack {
            QRScanner(onScanned: onScanned, onFailed: onFailed, onLost: onClose)
                .ignoresSafeArea()
                .navigationTitle("Scan the QR code")
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Close") { onClose() }
                            .accessibilityIdentifier("pairing-scanner-close")
                    }
                }
        }
    }
}

/// `DataScannerViewController` recognising QR codes, nothing else.
struct QRScanner: UIViewControllerRepresentable {
    let onScanned: @MainActor (String) -> Void
    /// The camera would not start: the sheet closes and the screen says so.
    let onFailed: @MainActor () -> Void
    /// The camera went away mid-scan: the sheet closes.
    let onLost: @MainActor () -> Void

    /// Whether this phone can scan: false in the simulator and on phones
    /// without the hardware. The scan button is hidden then.
    @MainActor
    static var isSupported: Bool { DataScannerViewController.isSupported }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced,
            recognizesMultipleItems: false,
            isHighFrameRateTrackingEnabled: false,
            isHighlightingEnabled: true
        )
        scanner.delegate = context.coordinator
        return scanner
    }

    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {
        context.coordinator.onScanned = onScanned
        context.coordinator.onLost = onLost
        if !scanner.isScanning {
            // The model checked the permission before opening the sheet;
            // this fails when the camera can't be used anyway (another app
            // holds it, a restriction).
            // Only a camera that can't be used is a failure; anything else
            // is tried again on the next update. Not during this view
            // update: the model changes after it.
            do { try scanner.startScanning() } catch {
                if !DataScannerViewController.isAvailable { Task { @MainActor in onFailed() } }
            }
        }
    }

    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning()
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onScanned: onScanned, onLost: onLost)
    }

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        var onScanned: @MainActor (String) -> Void
        var onLost: @MainActor () -> Void

        init(onScanned: @escaping @MainActor (String) -> Void, onLost: @escaping @MainActor () -> Void) {
            self.onScanned = onScanned
            self.onLost = onLost
        }

        /// The camera went away mid-scan (a call, another app): the sheet
        /// closes, and Scan tries again.
        func dataScanner(_ dataScanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) {
            onLost()
        }

        func dataScanner(
            _ dataScanner: DataScannerViewController, didAdd addedItems: [RecognizedItem], allItems: [RecognizedItem]
        ) {
            for item in addedItems {
                if case .barcode(let code) = item, let payload = code.payloadStringValue {
                    onScanned(payload)
                }
            }
        }
    }
}

/// The camera's permission, through AVFoundation.
@MainActor
public final class SystemCameraAccess: CameraAccess {
    public init() {}

    public var status: CameraStatus {
        switch AVCaptureDevice.authorizationStatus(for: .video) {
        case .authorized: .allowed
        case .notDetermined: .notAsked
        case .restricted: .restricted
        default: .refused
        }
    }

    public func request() async -> Bool {
        await AVCaptureDevice.requestAccess(for: .video)
    }
}

/// The pairing screen as a UIKit view controller, for the UIKit shell.
public enum PairingScreens {
    /// Whether this phone can scan a QR code at all.
    @MainActor
    public static var scannerSupported: Bool { QRScanner.isSupported }

    @MainActor
    public static func pairing(_ model: PairingModel, workspace: SignedInWorkspace?) -> UIViewController {
        UIHostingController(rootView: PairingView(model: model, workspace: workspace))
    }
}
