import Krowk
import KrowkKit

/// Which screen a launch opens, from what the key storage holds: no key is
/// onboarding; a key without the person's user keys is pairing (ticket 84;
/// until then "Signed in to <workspace>"); a paired phone never sees
/// onboarding.
public enum Launch: Equatable, Sendable {
    case signedOut
    case signedIn(SignedInWorkspace)
    case paired

    /// Reads it through `Account.open`, in Rust: Swift never reads the key's
    /// item itself. The first open after sign-in makes the device's keys.
    public static func read(api: String, storage: any KeyStorage) throws -> Launch {
        let account: Account
        do {
            account = try Account.open(api: api, storage: storage)
        } catch KrowkError.Refused(code: "signed_out", _) {
            return .signedOut
        }
        return try account.hasUserKeys()
            ? .paired
            : .signedIn(SignedInWorkspace(workspace: account.workspace(), name: account.workspaceName()))
    }
}
