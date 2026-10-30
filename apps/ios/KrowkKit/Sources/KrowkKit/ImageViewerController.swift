import UIKit

/// A prompt's image, full screen (R-IMG-15): drawn from bytes the device
/// holds, pinch to zoom, and a Close button or a tap dismisses it.
public final class ImageViewerController: UIViewController, UIScrollViewDelegate {
    public let image: UIImage
    public let scroll = UIScrollView()
    public let imageView: UIImageView
    public let closeButton = UIButton(configuration: .plain())

    public init(image: UIImage, number: UInt32) {
        self.image = image
        self.imageView = UIImageView(image: image)
        super.init(nibName: nil, bundle: nil)
        modalPresentationStyle = .fullScreen
        view.accessibilityIdentifier = "image-viewer"
        imageView.accessibilityLabel = "Image \(number)"
        imageView.isAccessibilityElement = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("ImageViewerController is built in code")
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        scroll.delegate = self
        scroll.minimumZoomScale = 1
        scroll.maximumZoomScale = 4
        scroll.showsHorizontalScrollIndicator = false
        scroll.showsVerticalScrollIndicator = false
        scroll.translatesAutoresizingMaskIntoConstraints = false
        imageView.contentMode = .scaleAspectFit
        imageView.translatesAutoresizingMaskIntoConstraints = false
        scroll.addSubview(imageView)
        view.addSubview(scroll)

        var close = UIButton.Configuration.plain()
        close.image = UIImage(systemName: "xmark")
        close.baseForegroundColor = .white
        closeButton.configuration = close
        closeButton.accessibilityLabel = "Close"
        closeButton.accessibilityIdentifier = "image-viewer-close"
        closeButton.translatesAutoresizingMaskIntoConstraints = false
        closeButton.addAction(UIAction { [weak self] _ in self?.dismiss(animated: true) }, for: .primaryActionTriggered)
        view.addSubview(closeButton)

        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        scroll.addGestureRecognizer(tap)

        NSLayoutConstraint.activate([
            scroll.topAnchor.constraint(equalTo: view.topAnchor),
            scroll.bottomAnchor.constraint(equalTo: view.bottomAnchor),
            scroll.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            imageView.topAnchor.constraint(equalTo: scroll.contentLayoutGuide.topAnchor),
            imageView.bottomAnchor.constraint(equalTo: scroll.contentLayoutGuide.bottomAnchor),
            imageView.leadingAnchor.constraint(equalTo: scroll.contentLayoutGuide.leadingAnchor),
            imageView.trailingAnchor.constraint(equalTo: scroll.contentLayoutGuide.trailingAnchor),
            imageView.widthAnchor.constraint(equalTo: scroll.frameLayoutGuide.widthAnchor),
            imageView.heightAnchor.constraint(equalTo: scroll.frameLayoutGuide.heightAnchor),
            closeButton.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 8),
            closeButton.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -8),
        ])
    }

    @objc private func tapped() {
        dismiss(animated: true)
    }

    public func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }
}
