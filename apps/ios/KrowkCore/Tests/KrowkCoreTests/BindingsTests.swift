// R-CLIENT-1: the phone reaches krowk-client through the UniFFI bindings,
// not a Swift copy of it. Each call below crosses into the Rust static
// library `make ios-core` built, on the simulator.
import Krowk
import Testing

@Suite("R-CLIENT-1: the Swift bindings call into the Rust core")
struct BindingsTests {
    @Test("R-CLIENT-1: protocolVersion() is the Rust core's protocol version")
    func protocolVersionIsTheCores() {
        // PROTOCOL_VERSION in krowk-client; moves with it.
        #expect(protocolVersion() == 1)
    }

    @Test("R-CLIENT-1: parsePairingCode(text:) normalizes a code in Rust")
    func parsePairingCodeNormalizes() throws {
        #expect(try parsePairingCode(text: "abcd efgh") == "ABCD-EFGH")
    }

    @Test("R-CLIENT-1: parsePairingCode(text:) throws KrowkError.InvalidInput for a short code")
    func parsePairingCodeRefusesAShortCode() {
        do {
            let code = try parsePairingCode(text: "abc")
            Issue.record("parsed \"abc\" as \(code)")
        } catch KrowkError.InvalidInput(let detail) {
            #expect(detail.contains("pairing code"))
        } catch {
            Issue.record("threw \(error), not KrowkError.InvalidInput")
        }
    }
}
