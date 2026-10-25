#if DEBUG
import Foundation

/// A session of 10,000 rows held in memory, for the memory budget (R-PERF-7,
/// ticket 127): what a DEBUG build opens with `KROWK_FIXTURE=tenk`, and
/// what BudgetUITests scrolls from top to bottom under XCTMemoryMetric.
/// Generated here from a fixed seed, the same every launch, never read
/// from the network: 2,000 turns, each a prompt, a reasoning block, a tool
/// call with its output (one in eight 16–64 KiB, the rest under 2 KiB),
/// an answer of one to three paragraphs, and the turn's end. Not in a
/// Release build (scripts/approvals.sh).
public enum TranscriptFixture {
    public static let sessionID = "0192f0a0-0000-7000-8000-00000000127f"
    /// The rows it makes: five a turn.
    public static let rows = 10_000
    /// The largest tool output, as the transcript's cells cap it.
    public static let largestOutput = 64 * 1024

    /// The session's events, as its log would hold them.
    public static func tenThousand(seed: UInt64 = 127) -> [TranscriptEvent] {
        var random = SplitMix64(seed: seed)
        var events: [TranscriptEvent] = []
        events.reserveCapacity(rows / 5 * 7)
        var n = 0
        func event(_ change: TranscriptChange) {
            n += 1
            events.append(TranscriptEvent(id: "fixture-\(n)", sessionID: sessionID, change: change))
        }
        for turn in 0..<(rows / 5) {
            let t = "t\(turn)"
            event(.turnStarted(turn: t))
            event(.userText(item: "u\(turn)", text: sentence(&random) + " " + sentence(&random), images: []))
            event(.reasoning(item: "r\(turn)", text: (0..<(1 + random.below(3))).map { _ in sentence(&random) }.joined(separator: " ")))
            let command = commands[random.below(commands.count)]
            event(.toolCall(item: "c\(turn)", callID: "call-\(turn)", name: "bash", input: #"{"command":"\#(command)"}"#))
            let size = random.below(8) == 0 ? 16 * 1024 + random.below(largestOutput - 16 * 1024 + 1) : 64 + random.below(2_048 - 64)
            event(.toolResult(item: "o\(turn)", callID: "call-\(turn)", output: output(bytes: size, &random), isError: false))
            let paragraphs = (0..<(1 + random.below(3))).map { _ in
                (0..<(3 + random.below(4))).map { _ in sentence(&random) }.joined(separator: " ")
            }
            event(.assistantText(item: "a\(turn)", text: paragraphs.joined(separator: "\n\n")))
            event(.turnCompleted(turn: t, end: .completed))
        }
        return events
    }

    static let commands = ["git status --short", "cargo test -p krowk-client", "ls -la src", "rg -n FrameDriver", "make check", "cat Cargo.lock"]
    static let words = ["the", "frame", "driver", "applies", "one", "batch", "per", "display", "tick", "and", "a", "streaming",
                        "answer", "grows", "in", "its", "own", "cell", "while", "list", "follows", "end", "host", "phone",
                        "relay", "session", "transcript", "budget", "memory", "scroll", "test", "build"]

    static func sentence(_ random: inout SplitMix64) -> String {
        var words = (0..<(6 + random.below(10))).map { _ in Self.words[random.below(Self.words.count)] }
        words[0] = words[0].capitalized
        return words.joined(separator: " ") + "."
    }

    /// Lines of a command's output, `bytes` long.
    static func output(bytes: Int, _ random: inout SplitMix64) -> String {
        var out = ""
        out.reserveCapacity(bytes)
        while out.utf8.count < bytes {
            out += "\(out.utf8.count)  src/\(words[random.below(words.count)])-\(random.below(10_000)).swift | \(random.below(400)) "
            out += String(repeating: "+", count: 1 + random.below(30)) + "\n"
        }
        return String(decoding: out.utf8.prefix(bytes), as: UTF8.self)
    }
}

/// A small seeded generator (SplitMix64): the same numbers on every run.
public struct SplitMix64: RandomNumberGenerator {
    private var state: UInt64

    public init(seed: UInt64) { state = seed }

    public mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }

    /// A number in `0..<n`.
    public mutating func below(_ n: Int) -> Int { Int(next() % UInt64(n)) }
}
#endif
