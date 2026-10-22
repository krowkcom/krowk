// R-PROTO-2, R-IOS-1, R-IOS-10, R-CLIENT-1: the fake fleet's demo session,
// opened on TestFleet's paired phone and followed through its relay, draws
// on the transcript screen: the session read at rest (and anything the
// host catches it up with) decoded by krowk-ffi into the generated
// LogEvents, drained into the model once per display frame, and drawn in
// order — the prompt, the answer, then the second turn's prompt, its tool
// call with the result beneath it, and its answer. No indicator shows over
// the drawn rows.
//
// Needs the fleet: apps/ios/scripts/with-fleet.sh hands its URLs to this
// process. Hosted in the app, as the Keychain needs.
import Foundation
import Krowk
import KrowkAccount
import KrowkKit
import Testing
import UIKit

extension HostedKeychain {
    @MainActor
    @Suite("R-PROTO-2, R-IOS-1, R-IOS-10: the demo session's transcript through the fleet")
    struct TranscriptTests {
        @Test("R-PROTO-2, R-IOS-1: the demo session's prompt, answer, and tool call with its result are drawn, in order, with no indicator over them")
        func demoDrawsInOrder() async throws {
            let opened = try await RelayConnectionTests.demo()
            let live = RelayConnection { opened }
            defer { live.stop() }
            let row = SessionRow(id: opened.id(), title: opened.title(), detail: "", live: true, accessibilityLabel: opened.title())
            let screen = TranscriptViewController(row: row, session: live)
            // Hosted in the app: a window on its scene.
            let scene = try #require(UIApplication.shared.connectedScenes.first as? UIWindowScene, "no window scene in the host app")
            let window = UIWindow(windowScene: scene)
            window.rootViewController = UINavigationController(rootViewController: screen)
            window.makeKeyAndVisible()
            defer { window.isHidden = true }
            #expect(screen.title == "A demo session")

            // The display link drains the session into the model; the
            // second turn's result is the last of what's asked for.
            let model = screen.model
            let deadline = ContinuousClock.now + .seconds(20)
            while Self.tool(in: model)?.output == nil && ContinuousClock.now < deadline {
                try await Task.sleep(for: .milliseconds(50))
            }
            try #require(Self.tool(in: model)?.output != nil, "no read with its result: \(Self.rows(of: model))")

            let rows = Self.rows(of: model)
            let order = [
                "user: Say hello",
                "assistant: Hello! I can read this project",
                "user: Please read the README",
                "tool: read README.md",
                "assistant: The README says this is the demo project.",
            ]
            var at = rows.startIndex
            for want in order {
                let found = try #require(rows[at...].firstIndex { $0.hasPrefix(want) }, "\(want) not after row \(at): \(rows)")
                at = found + 1
            }
            let tool = try #require(Self.tool(in: model))
            #expect(tool.isError == false)
            // `read` numbers its lines.
            #expect(tool.output?.contains("# The demo project") == true, "\(tool.output ?? "")")
            #expect(rows.contains("mark: Turn completed"), "\(rows)")

            // As drawn: the snapshot is the model's rows, and the tool's
            // cell shows its call and, beneath it, its result.
            // The snapshot follows the model a turn of the run loop later,
            // and the demo's held turn may still be coming in live.
            try await LiveFollowTests.wait(until: { screen.dataSource.snapshot().itemIdentifiers == model.ids }, within: .seconds(5), "the snapshot never caught up with the model")
            let path = try #require(screen.dataSource.indexPath(for: tool.id))
            screen.collectionView.scrollToItem(at: path, at: .centeredVertically, animated: false)
            window.layoutIfNeeded()
            let cell = try #require(screen.collectionView.cellForItem(at: path))
            #expect(cell.accessibilityIdentifier == "transcript-tool")
            let labels = Self.labels(in: cell)
            #expect(labels.contains("read README.md"), "\(labels)")
            #expect(labels.contains { $0.hasPrefix("✓ ") && $0.contains("# The demo project") }, "\(labels)")

            // R-IOS-1: rows drawn, no indicator anywhere on the screen.
            #expect(screen.drewRows)
            #expect(!Self.visibleIndicator(in: screen.view))
        }

        /// The rows as "<kind>: <text>", top to bottom.
        static func rows(of model: TranscriptModel) -> [String] {
            model.ids.compactMap { model.item($0) }.map { item in
                switch item.kind {
                case .user: "user: \(item.text)"
                case .assistant: "assistant: \(item.text)"
                case .reasoning: "reasoning: \(item.text)"
                case .tool: "tool: \(item.toolLine)"
                case .mark: "mark: \(item.text)"
                case .pending: "pending: \(item.text) (\(item.caption))"
                }
            }
        }

        /// The demo's `read` of the README.
        static func tool(in model: TranscriptModel) -> ItemState? {
            model.ids.compactMap { model.item($0) }.first { $0.kind == .tool && $0.toolName == "read" }
        }

        static func labels(in view: UIView) -> [String] {
            ((view as? UILabel)?.text.map { [$0] } ?? []) + view.subviews.flatMap { labels(in: $0) }
        }

        static func visibleIndicator(in view: UIView) -> Bool {
            if let spinner = view as? UIActivityIndicatorView, spinner.isAnimating, !spinner.isHidden { return true }
            return view.subviews.contains { visibleIndicator(in: $0) }
                || ((view as? UICollectionView)?.backgroundView.map { visibleIndicator(in: $0) } ?? false)
        }
    }
}
