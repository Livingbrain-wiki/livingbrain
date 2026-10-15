import Foundation
import XCTest
@testable import LivingBrainKit

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
