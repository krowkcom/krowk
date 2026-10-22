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
            ScannerSheet(onScanned: { model.scanned($0) }, onClose: { model.closeScanner() })
        }
    }

    private var scannerOpen: Binding<Bool> {
        Binding(get: { model.scanner == .open }, set: { if !$0 { model.closeScanner() } })
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
        if let sentence = model.failureSentence {
            // Never read as markdown: it can carry the other device's name.
            Text(Self.commandsStyled(sentence))
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
            if model.scanner == .refused {
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
            }
        }
        TextField("Code, like K7QF-9M3X", text: $model.codeText)
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

    /// `sentence` as plain text, its back-quoted commands in monospace.
    /// Nothing in it is parsed as markdown, so a device's name can't
    /// become a link.
    static func commandsStyled(_ sentence: String) -> AttributedString {
        var out = AttributedString()
        for (i, part) in sentence.split(separator: "`", omittingEmptySubsequences: false).enumerated() {
            var piece = AttributedString(String(part))
            if i % 2 == 1 { piece.inlinePresentationIntent = .code }
            out += piece
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
    let onClose: @MainActor () -> Void

    var body: some View {
        NavigationStack {
            QRScanner(onScanned: onScanned)
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
        if !scanner.isScanning {
            // Fails only when the camera can't be used, which the model
            // checked before opening the sheet.
            try? scanner.startScanning()
        }
    }

    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) {
        scanner.stopScanning()
    }

    func makeCoordinator() -> Coordinator {
        Coordinator(onScanned: onScanned)
    }

    @MainActor
    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        var onScanned: @MainActor (String) -> Void

        init(onScanned: @escaping @MainActor (String) -> Void) {
            self.onScanned = onScanned
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
