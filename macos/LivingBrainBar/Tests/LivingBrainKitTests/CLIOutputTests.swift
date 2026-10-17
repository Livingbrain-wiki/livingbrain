import Foundation
import XCTest
@testable import LivingBrainKit

/// The CLI prints one JSON object per line; a multi-line test literal is joined.
func oneLine(_ text: String) -> Data {
    Data(text.split(whereSeparator: \.isNewline).joined().utf8)
}

func fixture(_ name: String) throws -> Data {
    let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures"))
    return try Data(contentsOf: url)
}

final class CLIOutputTests: XCTestCase {
    func testDryRunReportDecodes() throws {
        let report = try CLIOutput.syncReport(fixture("sync-dry-run"))
        XCTAssertTrue(report.dryRun)
        XCTAssertEqual(report.scanned, 2)
        XCTAssertEqual(report.skipped, 1)
        XCTAssertEqual(report.unparsed, 1)
        XCTAssertEqual(report.notesWritten, 1)
        XCTAssertEqual(report.sessions.count, 2)
        let first = report.sessions[0]
        XCTAssertEqual(first.agent, "codex")
        XCTAssertEqual(first.status, "would_upload")
        XCTAssertEqual(first.toolCalls, 117)
        XCTAssertEqual(first.totals?.inputTokens, 3_108_722)
        XCTAssertNil(first.totals?.costUsd)
        XCTAssertEqual(report.summary, "1 would upload · 1 skipped · 1 note written")
    }

    func testUploadedReportDecodesAndIgnoresUnknownFields() throws {
        let report = try CLIOutput.syncReport(fixture("sync-uploaded"))
        XCTAssertFalse(report.dryRun)
        XCTAssertEqual(report.uploaded, 2)
        XCTAssertEqual(report.unchanged, 8)
        XCTAssertEqual(report.notesWritten, 2)
        XCTAssertEqual(report.sessions.first?.id, "01J00000000000000000000000")
        XCTAssertEqual(report.sessions.first?.totals?.costUsd, 0.42)
        XCTAssertEqual(report.summary, "2 uploaded · 8 unchanged · 30 skipped · 2 notes written")
    }

    func testReportAfterWrapperNoise() throws {
        var data = Data("wrapper: using production\n".utf8)
        data.append(try fixture("sync-uploaded"))
        XCTAssertEqual(try CLIOutput.syncReport(data).uploaded, 2)
    }

    func testEmptyStdoutIsAnError() {
        XCTAssertThrowsError(try CLIOutput.syncReport(Data()))
    }

    func testStatusAndAllowDecode() throws {
        let status = try CLIOutput.allowStatus(fixture("logs-status"))
        XCTAssertEqual(status.allowed, ["/Users/alice/code/atlas", "/Users/alice/code/zurich"])
        XCTAssertNil(status.changed)
        let allow = try CLIOutput.allowStatus(fixture("logs-allow"))
        XCTAssertEqual(allow.changed, true)
    }

    /// A sync that held a session for a workspace this Mac is not signed in to
    /// says so, and the message names the workspace and the count.
    func testSkippedNoTokenDecodesAsHeldSessions() throws {
        let json = #"""
        {"scanned":3,"uploaded":0,"unchanged":0,"skipped":1,"dry_run":false,
         "skipped_no_token":[{"workspace":"ws-chi","sessions":2}],
         "sessions":[{"agent":"codex","session_id":"a1","repo":"~/code/chi-web","status":"skipped_no_token",
                      "workspace":"ws-chi"}]}
        """#
        let report = try CLIOutput.syncReport(oneLine(json))
        XCTAssertEqual(report.skippedNoToken, [HeldSessions(workspace: "ws-chi", sessions: 2)])
        XCTAssertEqual(report.skippedNoToken.first?.message, "Sign in to ws-chi to sync 2 sessions")
        XCTAssertEqual(HeldSessions(workspace: "ws-chi", sessions: 1).message, "Sign in to ws-chi to sync 1 session")
        XCTAssertEqual(report.sessions.first?.workspace, "ws-chi")
    }

    /// A report from a CLI before routing has no held list and no workspace.
    func testOlderReportsHaveNoHeldSessions() throws {
        let report = try CLIOutput.syncReport(fixture("sync-uploaded"))
        XCTAssertEqual(report.skippedNoToken, [])
        XCTAssertNil(report.sessions.first?.workspace)
    }

    /// A session is synced per workspace: uploaded to one, it is still pending
    /// in another, and the default workspace keeps the bare key.
    func testLedgerIsKeyedByWorkspace() throws {
        let report = try CLIOutput.syncReport(oneLine(#"{"sessions":[{"agent":"codex","session_id":"a1","repo":"~/code/chi-web","status":"uploaded","workspace":"ws-chi","id":"src-1"}]}"#))
        var ledger = SessionLedger()
        ledger.record(report, at: Date(timeIntervalSince1970: 0))
        let routed = report.sessions[0]
        XCTAssertNotNil(ledger.entry(for: routed))
        var elsewhere = routed
        elsewhere.workspace = "default"
        XCTAssertNil(ledger.entry(for: elsewhere), "a re-routed session is pending in its new workspace")
        XCTAssertEqual(SessionLedger.key(for: elsewhere), "codex/a1")
    }

    /// Each allowed project carries its workspace; a project with none is the
    /// default, and a route's pick is shown as routed, never as its workspace.
    func testStatusProjectsCarryTheirWorkspace() throws {
        let json = #"""
        {"allowed":["/Users/alice/code/atlas","/Users/alice/code/chi-web"],
         "workspace":{"/Users/alice/code/atlas":"default","/Users/alice/code/chi-web":"ws-chi"},
         "routed":{"/Users/alice/code/atlas":"ws-owner"},"config":"/x"}
        """#
        let status = try CLIOutput.allowStatus(oneLine(json))
        XCTAssertEqual(status.projects, [
            SyncedProject(path: "/Users/alice/code/atlas", workspace: "default", routed: "ws-owner"),
            SyncedProject(path: "/Users/alice/code/chi-web", workspace: "ws-chi"),
        ])
        let older = try CLIOutput.allowStatus(fixture("logs-status"))
        XCTAssertTrue(older.projects.allSatisfy { $0.workspace == SyncedProject.defaultWorkspace && $0.routed == nil })
    }

    /// The opt-in file may name each root's workspace; an older file names none.
    func testAllowlistFileReadsWorkspaces() throws {
        let file = try CLIOutput.allowlistFile(Data(#"""
        {"version":1,"allowed":["/Users/alice/code/chi-web"],
         "workspace":{"/Users/alice/code/chi-web":"ws-chi"},
         "routes":[{"folder_glob":"chi-*","workspace":"ws-chi"}]}
        """#.utf8))
        XCTAssertEqual(file.workspace, ["/Users/alice/code/chi-web": "ws-chi"])
        let older = try CLIOutput.allowlistFile(fixture("allowlist-file"))
        XCTAssertNil(older.workspace)
    }

    func testAllowlistFile() throws {
        XCTAssertEqual(try CLIOutput.allowlistFile(fixture("allowlist-file")).allowed, ["/Users/alice/code/atlas"])
        XCTAssertThrowsError(try CLIOutput.allowlistFile(Data(#"{"version":2,"allowed":[]}"#.utf8))) { error in
            XCTAssertEqual(error as? CLIOutputError, .unsupportedAllowlistVersion(2))
        }
    }

    func testLoginPrompt() {
        let line = #"{"user_code":"ABCD-EFGH","verification_uri":"https://app.livingbrain.wiki/device","verification_uri_complete":"https://app.livingbrain.wiki/device?user_code=ABCD-EFGH"}"#
        let prompt = CLIOutput.loginPrompt(line)
        XCTAssertEqual(prompt?.userCode, "ABCD-EFGH")
        XCTAssertEqual(prompt?.openURL?.absoluteString, "https://app.livingbrain.wiki/device?user_code=ABCD-EFGH")
        XCTAssertNil(CLIOutput.loginPrompt("Open https://x and enter the code"))
        let bare = CLIOutput.loginPrompt(#"{"user_code":"X","verification_uri":"https://a/device","verification_uri_complete":null}"#)
        XCTAssertEqual(bare?.openURL?.absoluteString, "https://a/device")
    }

    func testFirstErrorLine() {
        XCTAssertEqual(
            CLIOutput.firstErrorLine(#"{"error":"not logged in — run `livingbrain login`"}"# + "\n"),
            "not logged in — run `livingbrain login`"
        )
        XCTAssertEqual(CLIOutput.firstErrorLine("\nerror: could not reach the API: dns\nmore"), "could not reach the API: dns")
        XCTAssertEqual(CLIOutput.firstErrorLine(#"{"error":"line one\nline two"}"#), "line one")
        XCTAssertNil(CLIOutput.firstErrorLine("  \n"))
    }

    func testNotLoggedInDetection() {
        XCTAssertTrue(CLIOutput.isNotLoggedIn("not logged in — run `livingbrain login`"))
        XCTAssertTrue(CLIOutput.isNotLoggedIn("invalid token — run `livingbrain login`"))
        XCTAssertFalse(CLIOutput.isNotLoggedIn("could not reach the API"))
    }

    func testScrubRemovesTokens() {
        let scrubbed = Scrub.text("Authorization: Bearer abc.def-123 and token=s3cr3t and lbp_0123456789abcdef")
        XCTAssertFalse(scrubbed.contains("abc.def-123"))
        XCTAssertFalse(scrubbed.contains("s3cr3t"))
        XCTAssertFalse(scrubbed.contains("lbp_0123"))
        let long = String(repeating: "a1B2", count: 12)
        XCTAssertFalse(Scrub.text("x \(long) y").contains(long))
        // Paths and ordinary messages survive.
        XCTAssertEqual(Scrub.text("could not read /Users/alice/.config/livingbrain/logs-allow.json"),
                       "could not read /Users/alice/.config/livingbrain/logs-allow.json")
    }
}
