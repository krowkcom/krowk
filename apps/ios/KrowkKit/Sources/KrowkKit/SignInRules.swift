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

    /// The longest name sent, in UTF-8 bytes: the registry takes 64
    /// characters, and krowk-ffi cuts the name to 64 bytes.
    public static let deviceNameLimit = 64

    /// What the approval page calls this phone: its name without control
    /// or format characters (the registry refuses the first; the second,
    /// like a right-to-left override, would make the name read otherwise
    /// on the page), cut to 64 bytes at a character's edge; "iPhone" when
    /// nothing is left. iOS 16 and later give an app the generic "iPhone"
    /// without a special entitlement.
    public static func deviceName(_ raw: String) -> String {
        var kept = String.UnicodeScalarView()
        var bytes = 0
        for scalar in raw.unicodeScalars {
            switch scalar.properties.generalCategory {
            case .control, .format: continue
            default: break
            }
            let width = String(scalar).utf8.count
            guard bytes + width <= deviceNameLimit else { break }
            kept.append(scalar)
            bytes += width
        }
        let name = String(kept).trimmingCharacters(in: .whitespaces)
        return name.isEmpty ? "iPhone" : name
    }

    /// The approval page, if the browser may open it: https, or plain http
    /// when the registry's own address is http (the terminal's
    /// `browsable_url`, which checks it first), and then only on the
    /// registry's own host. Never a page with a user or password in it.
    /// Anything else (`javascript:`, `ftp:`, a page with no host, http
    /// against an https registry) is nil and never opened.
    public static func browsablePage(_ raw: String, api: String) -> URL? {
        guard let url = URL(string: raw), let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
              let scheme = parts.scheme?.lowercased(), let host = parts.host, !host.isEmpty,
              parts.user == nil, parts.password == nil
        else { return nil }
        switch scheme {
        case "https":
            return url
        // As krowk-api's `Client::insecure`: the address as given.
        case "http" where api.hasPrefix("http://"):
            let apiHost = URLComponents(string: api)?.host
            return apiHost?.lowercased() == host.lowercased() ? url : nil
        default:
            return nil
        }
    }
}
