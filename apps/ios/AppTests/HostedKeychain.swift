// The hosted suites share the app's one Keychain (the `com.krowk.keys`
// service), and each test clears it as it starts. Swift Testing runs suites
// in parallel, so they are nested here, in one serialized suite: one test
// at a time across all of them. Each test also clears it as it ends, so
// what a test signed in as never reaches the app's next launch (the UI
// tests, the screenshot).
import Security
import Testing

@Suite(.serialized, .leavesNoKeys)
enum HostedKeychain {}

/// Deletes every `com.krowk.keys` item after each test.
struct LeavesNoKeys: SuiteTrait, TestTrait, TestScoping {
    var isRecursive: Bool { true }

    func provideScope(
        for test: Test, testCase: Test.Case?, performing function: @Sendable () async throws -> Void
    ) async throws {
        defer {
            let status = SecItemDelete([
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: "com.krowk.keys",
            ] as CFDictionary)
            if status != errSecSuccess && status != errSecItemNotFound {
                Issue.record("clearing com.krowk.keys: OSStatus \(status)")
            }
        }
        try await function()
    }
}

extension Trait where Self == LeavesNoKeys {
    static var leavesNoKeys: Self { Self() }
}
