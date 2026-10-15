import Foundation
import XCTest
@testable import LivingBrainKit

final class SessionFilesTests: XCTestCase {
    var root: URL!

    override func setUpWithError() throws {
        root = FileManager.default.temporaryDirectory.appendingPathComponent("lbfiles-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: root)
    }

    private func write(_ relative: String, _ text: String) throws -> String {
        let url = root.appendingPathComponent(relative)
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try text.write(to: url, atomically: true, encoding: .utf8)
        return url.path
    }

    func testIndexFindsBothAgents() throws {
        let claude = try write("claude/-Users-a-code-x/3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a.jsonl", "{}\n")
        let codex = try write("codex/2026/10/01/rollout-2026-10-01T09-31-00-a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d.jsonl", "{}\n")
        _ = try write("codex/2026/10/01/notes.txt", "x")
        let index = SessionFileIndex.scan(claudeRoot: root.appendingPathComponent("claude").path,
                                          codexRoot: root.appendingPathComponent("codex").path)
        XCTAssertEqual(index.path(agent: "claude_code", sessionId: "3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a"), claude)
        XCTAssertEqual(index.path(agent: "codex", sessionId: "a1b2c3d4-e5f6-4a9b-8c7d-3e5f7a9b1c2d"), codex)
        XCTAssertEqual(index.paths.count, 2)
        XCTAssertNil(SessionFileIndex.codexSessionId(fileName: "rollout-short.jsonl"))
    }

    func testMetaReadsLatestTimestampAndBranch() throws {
        let path = try write("s.jsonl", """
        {"type":"user","cwd":"/a","gitBranch":"main","timestamp":"2026-10-01T09:30:00.000Z"}
        {"type":"assistant","gitBranch":"feature/x","timestamp":"2026-10-01T09:45:10.500Z"}
        {"type":"pr-link","timestamp":"2026-10-01T09:40:00Z"}

        """)
        let meta = SessionFileMeta.read(path: path)
        XCTAssertEqual(meta.lastActivity, ISODate.parse("2026-10-01T09:45:10.500Z"))
        XCTAssertEqual(meta.branch, "main", "the branch the session started on, as the CLI records it")
    }

    func testMetaFallsBackToTheHeadForBranchAndMtimeForTime() throws {
        let filler = String(repeating: "{\"type\":\"x\"}\n", count: 200)
        let path = try write("codex.jsonl", "{\"type\":\"session_meta\",\"payload\":{\"git\":{\"branch\":\"dev\"}}}\n" + filler)
        let meta = SessionFileMeta.read(path: path, tailBytes: 64, headBytes: 4096)
        XCTAssertEqual(meta.branch, "dev")
        XCTAssertNotNil(meta.lastActivity, "the file's mtime stands in for a missing timestamp")
    }

    func testRepoNameUsesTheGitRoot() {
        let roots: Set<String> = ["/Users/a/code/atlas", "/Users/a/code/wt/feature"]
        let isRoot: (String) -> Bool = { roots.contains($0) }
        XCTAssertEqual(RepoName.display(repo: "~/code/atlas/crates/core", home: "/Users/a", isRepoRoot: isRoot), "atlas")
        XCTAssertEqual(RepoName.display(repo: "~/code/wt/feature", home: "/Users/a", isRepoRoot: isRoot), "feature")
        XCTAssertEqual(RepoName.display(repo: "~/scratch/notes", home: "/Users/a", isRepoRoot: isRoot), "notes")
        XCTAssertEqual(RepoName.display(repo: "~", home: "/Users/a", isRepoRoot: isRoot), "(home)")
        XCTAssertEqual(RepoName.display(repo: "/Users/a", home: "/Users/a", isRepoRoot: isRoot), "(home)")
        XCTAssertEqual(RepoName.display(repo: "/opt/x", home: "/Users/a", isRepoRoot: isRoot), "x")
    }

    func testRowsFillGapsFromTheFileAndSortNewestFirst() throws {
        // An older CLI: no file, branch or times in the report.
        let report = try CLIOutput.syncReport(fixture("sync-dry-run"))
        var index = SessionFileIndex()
        let older = try write("a.jsonl", "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"gitBranch\":\"old\"}\n")
        let newer = try write("b.jsonl", "{\"timestamp\":\"2026-06-01T00:00:00Z\",\"gitBranch\":\"new\"}\n")
        index = SessionFileIndex.scan(claudeRoot: "/nonexistent", codexRoot: "/nonexistent")
        XCTAssertTrue(index.paths.isEmpty)
        let files = ["codex/019d1127-b465-7a23-8678-31c5a98a39fe": older,
                     "claude_code/71767d6d-58a6-4d72-bc31-ed01f3f1d412": newer]
        var custom = SessionFileIndex()
        for (key, path) in files { custom.setPath(path, forKey: key) }
        let rows = SessionRow.rows(
            from: report, ledger: SessionLedger(), index: custom,
            meta: { SessionFileMeta.read(path: $0) }, home: "/Users/alice",
            isRepoRoot: { _ in false }, errors: [older: "offline"]
        )
        XCTAssertEqual(rows.map(\.branch), ["new", "old"])
        XCTAssertEqual(rows[0].repoName, "(home)")
        XCTAssertEqual(rows[1].repoName, "atlas")
        XCTAssertEqual(rows[1].status, .failed("offline"))
        XCTAssertEqual(rows[0].status, .notOptedIn)
    }
}
