import Foundation

/// The rules a sign-in keeps on the phone (R-IOS-9), as plain functions so
/// the tests reach them without a registry: which registry the app talks
/// to, what it calls itself, and which pages it lets the browser open.
public enum SignInRules {
    /// krowk's registry, `krowk_api::DEFAULT_BASE_URL`.
    public static let defaultAPI = "https://api.krowk.com/v1"

    /// The registry's `/v1` URL. A DEBUG build takes `KROWK_API_URL` from
    /// its launch environment, which is how the tests point it at the fake
    /// fleet; a release build always talks to krowk's registry.
    public static func apiURL(environment: [String: String] = ProcessInfo.processInfo.environment) -> String {
        #if DEBUG
        if let override = environment["KROWK_API_URL"], !override.isEmpty { return override }
        #endif
        return defaultAPI
    }

    /// The longest name the registry takes for a machine, in characters.
    public static let deviceNameLimit = 64

    /// What the approval page calls this phone: its name with the control
    /// characters dropped and cut to 64 characters, both of which the
    /// registry refuses; "iPhone" when nothing is left. iOS 16 and later
    /// give an app the generic "iPhone" without a special entitlement.
    public static func deviceName(_ raw: String) -> String {
        let kept = raw.unicodeScalars.filter { $0.properties.generalCategory != .control }
        let name = String(String.UnicodeScalarView(kept.prefix(deviceNameLimit)))
            .trimmingCharacters(in: .whitespaces)
        return name.isEmpty ? "iPhone" : name
    }

    /// The approval page, if the browser may open it: only https, or plain
    /// http when the registry's own address is http (the terminal's
    /// `browsable_url`). Anything else (`javascript:`, `ftp:`, a page with
    /// no host, http against an https registry) is nil and never opened.
    public static func browsablePage(_ raw: String, api: String) -> URL? {
        guard let url = URL(string: raw), let scheme = url.scheme?.lowercased(),
              let host = url.host(percentEncoded: false), !host.isEmpty
        else { return nil }
        switch scheme {
        case "https": return url
        case "http" where api.lowercased().hasPrefix("http://"): return url
        default: return nil
        }
    }
}
