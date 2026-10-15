import Foundation
import XCTest
@testable import LivingBrainKit

final class IdleDebouncerTests: XCTestCase {
    let t0 = Date(timeIntervalSince1970: 1_000_000)

    func testOnlySessionLogsAreTracked() {
        var debouncer = IdleDebouncer(idle: 120)
        debouncer.touch("/x/.claude/projects/a/s.jsonl", at: t0)
        debouncer.touch("/x/.claude/projects/a/notes.txt", at: t0)
        XCTAssertEqual(debouncer.pending.count, 1)
    }

    func testAFileIsDueAfterItGoesQuiet() {
        var debouncer = IdleDebouncer(idle: 120)
        debouncer.touch("a.jsonl", at: t0)
        debouncer.touch("a.jsonl", at: t0.addingTimeInterval(60))
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(150)), [], "the second write restarted the clock")
        XCTAssertEqual(debouncer.nextDue(), t0.addingTimeInterval(180))
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(180)), ["a.jsonl"])
        XCTAssertTrue(debouncer.pending.isEmpty)
        XCTAssertNil(debouncer.nextDue())
    }

    func testOldestFirstAndATouchedFileComesBack() {
        var debouncer = IdleDebouncer(idle: 10)
        debouncer.touch("b.jsonl", at: t0.addingTimeInterval(5))
        debouncer.touch("a.jsonl", at: t0)
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(100)), ["a.jsonl", "b.jsonl"])
        debouncer.touch("a.jsonl", at: t0.addingTimeInterval(200))
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(205)), [])
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(210)), ["a.jsonl"])
    }

    func testAnOutOfOrderEventDoesNotRewindTheClock() {
        var debouncer = IdleDebouncer(idle: 10)
        debouncer.touch("a.jsonl", at: t0.addingTimeInterval(50))
        debouncer.touch("a.jsonl", at: t0)
        XCTAssertEqual(debouncer.takeDue(now: t0.addingTimeInterval(55)), [])
    }
}

final class NotificationThrottleTests: XCTestCase {
    let t0 = Date(timeIntervalSince1970: 2_000_000)

    func testUploadsInsideTheWindowAreHeldAndFolded() {
        var throttle = NotificationThrottle(upload: 600, error: 1800)
        XCTAssertNil(throttle.upload(count: 0, now: t0))
        XCTAssertEqual(throttle.upload(count: 2, now: t0), 2)
        XCTAssertNil(throttle.upload(count: 1, now: t0.addingTimeInterval(60)))
        XCTAssertNil(throttle.upload(count: 3, now: t0.addingTimeInterval(120)))
        XCTAssertEqual(throttle.upload(count: 1, now: t0.addingTimeInterval(601)), 5)
        XCTAssertEqual(throttle.heldUploads, 0)
    }

    func testTheSameErrorWaitsANewOneDoesNot() {
        var throttle = NotificationThrottle(upload: 600, error: 1800)
        XCTAssertTrue(throttle.error("offline", now: t0))
        XCTAssertFalse(throttle.error("offline", now: t0.addingTimeInterval(60)))
        XCTAssertTrue(throttle.error("not logged in", now: t0.addingTimeInterval(61)))
        XCTAssertTrue(throttle.error("offline", now: t0.addingTimeInterval(62)))
        XCTAssertTrue(throttle.error("offline", now: t0.addingTimeInterval(62 + 1800)))
    }
}

final class SessionsTests: XCTestCase {
    func testDryRunV2CarriesLocalFields() throws {
        let report = try CLIOutput.syncReport(fixture("sync-dry-run-v2"))
        XCTAssertEqual(report.sessions.count, 3)
        let demo = try XCTUnwrap(report.sessions.first { $0.sessionId.hasPrefix("3f2a9c1e") })
        XCTAssertEqual(demo.branch, "main")
        XCTAssertEqual(demo.status, "would_upload")
        XCTAssertEqual(demo.note?.slug, "agent-log-claude-code-3f2a9c1e-5b7d-4e8a-9c1f-2a4b6d8e0f1a")
        XCTAssertEqual(demo.note?.status, "would_write")
        XCTAssertTrue(demo.file?.hasSuffix(".jsonl") == true)
        XCTAssertNotNil(demo.startedDate)
        XCTAssertEqual(demo.projectName, "demo")
        XCTAssertEqual(demo.totalTokens, 120 + 45 + 30 + 15)
        let side = try XCTUnwrap(report.sessions.first { $0.repo == "~/code/side" })
        XCTAssertFalse(side.isOptedIn)
        XCTAssertNil(side.note)
    }

    func testShowDecodesTheRecord() throws {
        let shown = try CLIOutput.sessionShow(fixture("logs-show"))
        XCTAssertFalse(shown.optedIn)
        let record = try XCTUnwrap(shown.record)
        XCTAssertEqual(record.agent, "claude_code")
        XCTAssertEqual(record.branch, "main")
        XCTAssertEqual(record.turns.count, 2)
        XCTAssertEqual(record.turns[1].toolCalls, ["Bash"])
        XCTAssertEqual(record.turns.map(\.id), [0, 1])

        let raw = try CLIOutput.sessionShow(Data(#"{"file":"/f","opted_in":true,"redacted":1,"record":null,"body":"{\"x\":\"\\[REDACTED:email]\"}"}"#.utf8))
        XCTAssertNil(raw.record)
        XCTAssertEqual(SessionShow.repairEscapes(#"a\[b \\[c \n \"q\""#), #"a\\[b \\[c \n \"q\""#)
        let broken = try CLIOutput.sessionShow(Data(#"{"file":"/f","opted_in":true,"redacted":1,"record":null,"body":"{\"session_id\":\"s\",\"turns\":[{\"role\":\"user\",\"text\":\"mail\\[REDACTED:email]\",\"tool_calls\":[]}]}"}"#.utf8))
        XCTAssertEqual(broken.displayRecord?.turns.first?.text, #"mail\[REDACTED:email]"#)
        XCTAssertEqual(raw.body, #"{"x":"\[REDACTED:email]"}"#)
    }

    func testUnsupportedDetection() {
        XCTAssertTrue(CLIOutput.isUnsupported("error: unexpected argument '--session' found\n"))
        XCTAssertTrue(CLIOutput.isUnsupported("error: unrecognized subcommand 'show'"))
        XCTAssertFalse(CLIOutput.isUnsupported("error: not logged in"))
    }

    func testLedgerRecordsOnlyRealSyncs() throws {
        var ledger = SessionLedger()
        let at = Date(timeIntervalSince1970: 3_000_000)
        ledger.record(try CLIOutput.syncReport(fixture("sync-dry-run-v2")), at: at)
        XCTAssertTrue(ledger.entries.isEmpty, "a dry run proves nothing reached the server")
        ledger.record(try CLIOutput.syncReport(fixture("sync-uploaded")), at: at)
        let entry = try XCTUnwrap(ledger.entries["claude_code/a"])
        XCTAssertEqual(entry.sourceId, "01J00000000000000000000000")
        XCTAssertEqual(entry.noteSlug, "agent-log-claude_code-a")

        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString).appendingPathComponent("ledger.json")
        try ledger.save(to: url)
        XCTAssertEqual(SessionLedger.load(from: url), ledger)
        XCTAssertEqual(SessionLedger.load(from: url.appendingPathExtension("missing")), SessionLedger())
    }

    func testRowsStatusAndLinks() throws {
        let report = try CLIOutput.syncReport(fixture("sync-dry-run-v2"))
        var ledger = SessionLedger()
        var uploaded = try CLIOutput.syncReport(fixture("sync-dry-run-v2"))
        uploaded.dryRun = false
        uploaded.sessions = uploaded.sessions.filter(\.isOptedIn).prefix(1).map {
            var s = $0; s.status = "uploaded"; return s
        }
        let at = Date(timeIntervalSince1970: 4_000_000)
        ledger.record(uploaded, at: at)

        let rows = SessionRow.rows(from: report, ledger: ledger)
        XCTAssertEqual(rows.count, 3)
        let base = URL(string: "https://app.livingbrain.wiki/")!
        let synced = try XCTUnwrap(rows.first { $0.status == .synced(at) })
        XCTAssertEqual(synced.webURL(base: base).absoluteString,
                       "https://app.livingbrain.wiki/wiki.html?slug=\(synced.session.note!.slug!)")
        let skipped = try XCTUnwrap(rows.first { $0.status == .notOptedIn })
        XCTAssertEqual(skipped.webURL(base: base).absoluteString,
                       "https://app.livingbrain.wiki/wiki.html?view=sessions&project=side")
        XCTAssertEqual(rows.filter { $0.status == .pending }.count, 1)
        // Newest first.
        let ends = rows.compactMap(\.session.endedDate)
        XCTAssertEqual(ends, ends.sorted(by: >))
    }

    func testRepoSummaries() throws {
        let report = try CLIOutput.syncReport(fixture("sync-dry-run-v2"))
        let repos = RepoSummary.summarise(
            sessions: report.sessions,
            allowed: ["/Users/alice/code/demo", "/Users/alice/elsewhere"],
            home: "/Users/alice"
        )
        let demo = try XCTUnwrap(repos.first { $0.name == "demo" })
        XCTAssertEqual(demo.sessions, 2)
        XCTAssertTrue(demo.optedIn)
        let side = try XCTUnwrap(repos.first { $0.name == "side" })
        XCTAssertFalse(side.optedIn)
        let elsewhere = try XCTUnwrap(repos.first { $0.name == "elsewhere" })
        XCTAssertTrue(elsewhere.optedIn)
        XCTAssertEqual(elsewhere.sessions, 0)
        XCTAssertEqual(repos.prefix(2).map(\.optedIn), [true, true], "opted-in repos sort first")
        XCTAssertTrue(RepoSummary.isInside("/a/b/c", "/a/b"))
        XCTAssertFalse(RepoSummary.isInside("/a/bc", "/a/b"))
        XCTAssertEqual(RepoSummary.expand("~/x", home: "/h"), "/h/x")
        XCTAssertEqual(RepoSummary.expand("~", home: "/h"), "/h")
    }
}
