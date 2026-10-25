import Foundation

// A session's model from the phone (ticket 135, R-SWITCH-4, R-LAG-8,
// R-INST-6): which model it runs on, as its log says; a switch the phone
// asked for, shown at once and settled by the host; the host's offer of
// another instance when one hits its limit.

/// A model on one of the host's instances: the generated `ModelRef`'s two
/// fields, which KrowkAccount maps both ways. KrowkKit does not import the
/// bindings.
public struct ModelChoice: Hashable, Sendable {
    /// The instance's name: "anthropic", "anthropic:personal".
    public var instance: String
    /// The model id on it.
    public var model: String

    public init(instance: String, model: String) {
        self.instance = instance
        self.model = model
    }

    /// `<instance>/<id>`, as the terminal writes it.
    public var name: String { "\(instance)/\(model)" }

    /// `name`, cleaned for one line on screen.
    public var shown: String { SafeText.line(name[...], limit: 120) }
}

/// An instance at its limit, and where the host says the session could go
/// on (`SwitchOffer`, R-INST-7).
public struct SwitchOfferInfo: Equatable, Sendable {
    public var from: ModelChoice
    public var to: ModelChoice
    /// When `from`'s limit lifts, in milliseconds since the epoch, when the
    /// provider said.
    public var resetsAtMs: Int64?

    public init(from: ModelChoice, to: ModelChoice, resetsAtMs: Int64? = nil) {
        self.from = from
        self.to = to
        self.resetsAtMs = resetsAtMs
    }
}

/// An offer the transcript holds: the limited turn's, and the prompt to
/// send again on Continue, when this phone sent that turn's prompt.
public struct HeldOffer: Equatable, Sendable {
    public var turn: String
    public var info: SwitchOfferInfo
    /// The limited turn's prompt, when it was this phone's: Continue sends
    /// it again. Nil for another device's turn: the banner only says the
    /// limit.
    public var again: String?

    /// The banner's line: `<from> is at its limit — continue on <to>?`, or
    /// only the limit when there's nothing to continue.
    public var line: String {
        again == nil ? "\(info.from.shown) is at its limit" : "\(info.from.shown) is at its limit — continue on \(info.to.shown)?"
    }
}

/// How near its limit an instance is, as a session's `limits` line last
/// said (R-INST-6).
public struct InstanceLimit: Equatable, Sendable {
    /// "allowed", "warning" or "limited".
    public var state: String
    public var usedPercent: Double?
    public var resetsAtMs: Int64?

    public init(state: String, usedPercent: Double? = nil, resetsAtMs: Int64? = nil) {
        self.state = state
        self.usedPercent = usedPercent
        self.resetsAtMs = resetsAtMs
    }
}

/// What the navigation bar's subtitle says of the model.
public enum ModelLine {
    /// `switching to <model>…` while a switch is out, else the current
    /// model; empty when neither is known yet.
    public static func text(current: ModelChoice?, switching: ModelChoice?) -> String {
        if let switching { return "switching to \(switching.shown)…" }
        return current?.shown ?? ""
    }
}
