import PhotosUI
import UIKit
import UniformTypeIdentifiers

/// The photo picker's answer (R-IMG-14): each chosen photo's bytes as the
/// library holds them (HEIC, usually), handed to `picked` in the order
/// chosen. The composer normalizes them. A photo that can't be loaded is
/// left out; the picker says nothing of it.
@MainActor
final class PhotoPickerDelegate: NSObject, PHPickerViewControllerDelegate {
    private let picked: @MainActor ([Data]) async -> Void

    init(picked: @escaping @MainActor ([Data]) async -> Void) {
        self.picked = picked
    }

    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        let providers = results.map(\.itemProvider)
        guard !providers.isEmpty else { return }
        Task { @MainActor in
            var photos: [Data] = []
            for provider in providers {
                if let data = await Self.load(provider) { photos.append(data) }
            }
            await picked(photos)
        }
    }

    private static func load(_ provider: NSItemProvider) async -> Data? {
        await withCheckedContinuation { (found: CheckedContinuation<Data?, Never>) in
            _ = provider.loadDataRepresentation(forTypeIdentifier: UTType.image.identifier) { data, _ in
                found.resume(returning: data)
            }
        }
    }
}
