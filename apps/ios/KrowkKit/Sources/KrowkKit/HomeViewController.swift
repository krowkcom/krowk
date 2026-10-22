import UIKit

/// The large title "krowk" and the protocol version the Rust core speaks:
/// what the app shows while it hosts the unit tests, which keep the
/// Keychain to themselves. A paired phone opens on Sessions. The version comes in from the app target,
/// which reads it through the bindings, so KrowkKit never imports them.
public final class HomeViewController: UIViewController {
    private let protocolVersion: UInt32

    /// "Protocol <n>"; the UI test finds it by its identifier.
    let versionLabel = UILabel()

    public init(protocolVersion: UInt32) {
        self.protocolVersion = protocolVersion
        super.init(nibName: nil, bundle: nil)
        title = "krowk"
        navigationItem.largeTitleDisplayMode = .always
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("HomeViewController is built in code")
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground

        versionLabel.text = "Protocol \(protocolVersion)"
        versionLabel.font = .preferredFont(forTextStyle: .body)
        versionLabel.adjustsFontForContentSizeCategory = true
        versionLabel.textColor = .secondaryLabel
        versionLabel.accessibilityIdentifier = "protocol-version"
        versionLabel.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(versionLabel)

        let guide = view.layoutMarginsGuide
        NSLayoutConstraint.activate([
            versionLabel.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            versionLabel.trailingAnchor.constraint(lessThanOrEqualTo: guide.trailingAnchor),
            versionLabel.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
        ])
    }
}
