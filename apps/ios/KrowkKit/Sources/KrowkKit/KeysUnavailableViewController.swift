import UIKit

/// What the app shows instead of the home screen when it could not open its
/// Keychain-backed key storage (R-IOS-8): on an iPhone with no Secure
/// Enclave, krowk keeps no keys, so it cannot sign in, pair or join the
/// relay, and says why rather than falling back to a weaker store.
public final class KeysUnavailableViewController: UIViewController {
    private let reason: String

    /// The headline and the reason; the UI test finds the reason by its
    /// identifier.
    let headline = UILabel()
    let reasonLabel = UILabel()

    public init(reason: String) {
        self.reason = reason
        super.init(nibName: nil, bundle: nil)
        title = "krowk"
        navigationItem.largeTitleDisplayMode = .always
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("KeysUnavailableViewController is built in code")
    }

    override public func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .systemBackground

        headline.text = "This iPhone can't keep krowk's keys safely."
        headline.font = .preferredFont(forTextStyle: .headline)
        headline.adjustsFontForContentSizeCategory = true
        headline.numberOfLines = 0

        reasonLabel.text = reason
        reasonLabel.font = .preferredFont(forTextStyle: .body)
        reasonLabel.adjustsFontForContentSizeCategory = true
        reasonLabel.textColor = .secondaryLabel
        reasonLabel.numberOfLines = 0
        reasonLabel.accessibilityIdentifier = "keys-unavailable"

        let stack = UIStackView(arrangedSubviews: [headline, reasonLabel])
        stack.axis = .vertical
        stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(stack)

        let guide = view.layoutMarginsGuide
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: guide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: guide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 16),
        ])
    }
}
