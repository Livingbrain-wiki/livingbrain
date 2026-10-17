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
        // A held (mixed or unroutable) session never shares the bare default key.
        var mixed = routed
        mixed.workspace = "mixed"
        XCTAssertNotEqual(SessionLedger.key(for: mixed), "codex/a1")
        XCTAssertNil(ledger.entry(for: mixed))
    }

    /// Each allowed project carries its explicit choice (nil when automatic)
    /// and its effective destination, with the route that chose it.
    func testStatusProjectsCarryExplicitAndEffective() throws {
        let json = #"""
        {"allowed":["/Users/alice/code/atlas","/Users/alice/code/chi-web","/Users/alice/code/pinned"],
         "workspace":{"/Users/alice/code/atlas":"default","/Users/alice/code/chi-web":"default","/Users/alice/code/pinned":"default"},
         "workspace_explicit":{"/Users/alice/code/atlas":null,"/Users/alice/code/chi-web":null,"/Users/alice/code/pinned":"default"},
         "workspace_effective":{"/Users/alice/code/atlas":"ws-owner","/Users/alice/code/chi-web":"unroutable","/Users/alice/code/pinned":"default"},
         "route_reason":{"/Users/alice/code/atlas":"remote_owner CHI-Ecosystem on github.com","/Users/alice/code/chi-web":"the remote is on gitlab.com","/Users/alice/code/pinned":null},
         "config":"/x"}
        """#
        let status = try CLIOutput.allowStatus(oneLine(json))
        XCTAssertEqual(status.projects, [
            SyncedProject(path: "/Users/alice/code/atlas", explicit: nil, effective: "ws-owner",
                          routeReason: "remote_owner CHI-Ecosystem on github.com"),
            SyncedProject(path: "/Users/alice/code/chi-web", explicit: nil, effective: "unroutable",
                          routeReason: "the remote is on gitlab.com"),
            SyncedProject(path: "/Users/alice/code/pinned", explicit: "default", effective: "default"),
        ])
        // An older CLI names no explicit choices: every project is automatic and default.
        let older = try CLIOutput.allowStatus(fixture("logs-status"))
        XCTAssertTrue(older.projects.allSatisfy { $0.explicit == nil && $0.effective == SyncedProject.defaultWorkspace })
    }

    /// A folder that holds checkouts is per-repository and lists each checkout's
    /// own answer; a child that follows a parent says so, and a held repository
    /// carries its own top level for "Pin This Repository".
    func testStatusProjectsCarryParentAndNestedAnswers() throws {
        let json = #"""
        {"allowed":["/work","/work/app","/work/chi-web"],
         "workspace_explicit":{"/work":null,"/work/app":null,"/work/chi-web":null},
         "workspace_effective":{"/work":"per-repository","/work/app":"ws-team","/work/chi-web":"unroutable"},
         "route_reason":{"/work":"per-repository","/work/app":null,"/work/chi-web":"the remote is on gitlab.com"},
         "explicit_from":{"/work":null,"/work/app":"/work/team","/work/chi-web":null},
         "repo_top":{"/work":null,"/work/app":null,"/work/chi-web":"/work/chi-web"},
         "repos":{"/work":[{"path":"/work/notes","effective":"default","route_reason":null}]}}
        """#
        let status = try CLIOutput.allowStatus(oneLine(json))
        let folder = status.projects[0]
        XCTAssertEqual(folder.effective, "per-repository")
        XCTAssertEqual(folder.nested, [NestedRepo(path: "/work/notes", effective: "default")])
        XCTAssertEqual(status.projects[1].inheritedFrom, "/work/team")
        XCTAssertEqual(status.projects[2].repoTop, "/work/chi-web")
    }

    /// Pinning never widens consent: the allowed path and checkouts under it may
    /// be pinned (the latter at the allowed path's own detail level); a parent of
    /// the allowed path, a sibling that only shares a name prefix, or an unrelated
    /// path is refused.
    func testPinActionNeverWidensConsent() throws {
        let project = SyncedProject(path: "/repo/src", detail: "summary")
        XCTAssertEqual(project.pinAction(for: "/repo/src"), .allowedRoot)
        XCTAssertEqual(project.pinAction(for: "/repo/src/app"), .nestedCheckout)
        XCTAssertEqual(project.pinAction(for: "/repo"), .refused, "a parent would widen what syncs")
        XCTAssertEqual(project.pinAction(for: "/repo/srcx"), .refused, "a name prefix is not inside")
        XCTAssertEqual(project.pinAction(for: "/elsewhere"), .refused)

        let json = #"""
        {"allowed":["/repo/src"],"detail":{"/repo/src":"summary"},"config":"/x"}
        """#
        let status = try CLIOutput.allowStatus(oneLine(json))
        XCTAssertEqual(status.projects.first?.detail, "summary")
    }

    /// A version-2 file (routing) is read; a version this build does not know is not.
    func testAllowlistFileAcceptsVersionTwo() throws {
        let file = try CLIOutput.allowlistFile(Data(#"{"version":2,"allowed":["/x"],"routes":[]}"#.utf8))
        XCTAssertEqual(file.version, 2)
        XCTAssertThrowsError(try CLIOutput.allowlistFile(Data(#"{"version":3,"allowed":[]}"#.utf8)))
    }

    /// A keychain refusal is reported apart from a missing sign-in, and the
    /// sessions it holds are not offered a sign-in button.
    func testKeychainDeniedIsReportedApart() throws {
        let json = #"""
        {"skipped_no_token":[{"workspace":"ws-a","sessions":1}],
         "skipped_keychain_denied":[{"workspace":"ws-b","sessions":3}],
         "skipped_unroutable":2,"sessions":[]}
        """#
        let report = try CLIOutput.syncReport(oneLine(json))
        XCTAssertEqual(report.skippedNoToken, [HeldSessions(workspace: "ws-a", sessions: 1)])
        XCTAssertEqual(report.skippedKeychainDenied, [HeldSessions(workspace: "ws-b", sessions: 3, denied: true)])
        XCTAssertEqual(report.skippedUnroutable, 2)
        XCTAssertEqual(report.held.count, 2)
        XCTAssertTrue(report.held[1].message.contains("Keychain"), report.held[1].message)
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
        XCTAssertThrowsError(try CLIOutput.allowlistFile(Data(#"{"version":3,"allowed":[]}"#.utf8))) { error in
            XCTAssertEqual(error as? CLIOutputError, .unsupportedAllowlistVersion(3))
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
