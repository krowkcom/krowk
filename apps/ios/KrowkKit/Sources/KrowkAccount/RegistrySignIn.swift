import Krowk
import KrowkKit

/// KrowkKit's `SignInService` over krowk-ffi's `SignIn` (R-IOS-9,
/// R-CLIENT-1): Rust opens the sign-in, polls with the terminal's policy,
/// and writes the key straight into `storage`, the app's
/// `KeychainKeyStorage`, which sees its bytes only to seal them. The model
/// and the screens never hold the key: `wait` hands back the workspace
/// only (R-CRED-1: krowk's own key, and nothing of a provider's).
public struct RegistrySignIn: SignInService {
    public let api: String
    private let storage: any KeyStorage

    public init(api: String, storage: any KeyStorage) {
        self.api = api
        self.storage = storage
    }

    public func start(deviceName: String) throws(SignInFailure) -> any SignInAttempt {
        do {
            return Attempt(signIn: try SignIn.start(api: api, deviceName: deviceName), storage: storage)
        } catch {
            throw SignInFailure(error)
        }
    }

    private struct Attempt: SignInAttempt {
        let signIn: SignIn
        let storage: any KeyStorage

        var code: String { signIn.code() }
        var verificationURL: String { signIn.verificationUrl() }

        func wait() throws(SignInFailure) -> SignedInWorkspace {
            do {
                let who = try signIn.wait(storage: storage)
                return SignedInWorkspace(workspace: who.workspace, name: who.workspaceName)
            } catch {
                throw SignInFailure(error)
            }
        }

        func cancel() {
            signIn.cancel()
        }
    }
}

extension SignInFailure {
    /// The core's refusal codes, as the terminal's `krowk login` names them.
    init(_ error: any Error) {
        guard let error = error as? KrowkError else {
            self = .other(detail: String(describing: error))
            return
        }
        switch error {
        case .Refused(code: "authorization_denied", _): self = .denied
        case .Refused(code: "authorization_expired", _): self = .expired
        case .Refused(code: "credentials_unwritable", let detail): self = .keychainRefused(detail: detail)
        case .Refused(code: "unsafe_url", _): self = .unsafePage
        case .Refused(code: "cancelled", _): self = .cancelled
        case .Network(let detail): self = .network(detail: detail)
        case .Refused(_, let detail), .InvalidInput(let detail), .Internal(let detail), .Decode(let detail):
            self = .other(detail: detail)
        }
    }
}
