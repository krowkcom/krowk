import Foundation
import KrowkKit
import LocalAuthentication

/// The owner check before an allow (R-IOS-11): Face ID, falling back to
/// the device passcode (`deviceOwnerAuthentication`), with the Face ID
/// usage sentence the Info.plist gives (`NSFaceIDUsageDescription`;
/// without it, asking would end the app). No passcode set, a failure or a
/// cancel all say no, and nothing is sent.
struct DeviceOwnerCheck: OwnerCheck {
    @MainActor func confirm(reason: String) async -> Bool {
        let context = LAContext()
        var unable: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &unable) else { return false }
        return await withCheckedContinuation { (done: CheckedContinuation<Bool, Never>) in
            context.evaluatePolicy(.deviceOwnerAuthentication, localizedReason: reason) { yes, _ in
                done.resume(returning: yes)
            }
        }
    }
}

/// Which owner check the app uses: `LAContext`'s, always, in a release
/// build. A DEBUG build's UI tests choose a stub that answers yes or no
/// with the launch argument `-KrowkOwnerCheck yes|no`, since a simulator
/// has no enrolled face; the stub and the argument are compiled only under
/// DEBUG (ios.yml checks a Release build has neither).
enum OwnerChecks {
    static func make(arguments: [String] = ProcessInfo.processInfo.arguments) -> any OwnerCheck {
        #if DEBUG
        if let at = arguments.firstIndex(of: "-KrowkOwnerCheck"), arguments.indices.contains(at + 1) {
            return StubOwnerCheck(answer: arguments[at + 1] == "yes")
        }
        #endif
        return DeviceOwnerCheck()
    }
}

#if DEBUG
/// The UI tests' owner check: says what the launch argument said, at once.
struct StubOwnerCheck: OwnerCheck {
    let answer: Bool
    @MainActor func confirm(reason: String) async -> Bool { answer }
}
#endif
