import Foundation
import XCTest
@testable import LivingBrainKit

final class SyncStateTests: XCTestCase {
    func testBeginRefusesOverlap() {
        var state = SyncState()
        XCTAssertTrue(state.begin())
        XCTAssertEqual(state.phase, .syncing)
        XCTAssertFalse(state.begin())
    }

    func testSuccessRecordsReport() throws {
        var state = SyncState()
        state.begin()
        let at = Date(timeIntervalSince1970: 1_000)
        state.succeed(try CLIOutput.syncReport(fixture("sync-uploaded")), at: at)
        XCTAssertEqual(state.phase, .idle)
        XCTAssertEqual(state.lastSyncAt, at)
        XCTAssertEqual(state.lastReport?.uploaded, 2)
    }

    func testNotLoggedInFailureAsksForSignIn() {
        var state = SyncState()
        state.begin()
        state.fail(stderr: #"{"error":"not logged in — run `livingbrain login`"}"#, status: 1)
        XCTAssertTrue(state.needsSignIn)
        XCTAssertEqual(state.errorMessage, "not logged in — run `livingbrain login`")
        state.signedIn()
        XCTAssertFalse(state.needsSignIn)
        XCTAssertEqual(state.phase, .idle)
    }

    func testOtherFailureKeepsSignInHidden() {
        var state = SyncState()
        state.begin()
        state.fail(stderr: "", status: 3)
        XCTAssertFalse(state.needsSignIn)
        XCTAssertEqual(state.errorMessage, "livingbrain exited with status 3")
        XCTAssertTrue(state.begin(), "an error does not block the next sync")
    }

    func testLastSyncLine() {
        var state = SyncState()
        XCTAssertEqual(state.lastSyncLine(), "Not synced yet")
        let now = Date(timeIntervalSince1970: 10_000)
        state.succeed(SyncReport(), at: now.addingTimeInterval(-10))
        XCTAssertEqual(state.lastSyncLine(now: now), "Last sync just now")
        state.succeed(SyncReport(), at: now.addingTimeInterval(-600))
        XCTAssertEqual(state.lastSyncLine(now: now, locale: Locale(identifier: "en_US")), "Last sync 10 minutes ago")
    }

    func testAutoSyncDue() {
        let now = Date(timeIntervalSince1970: 100_000)
        XCTAssertTrue(AutoSync.isDue(lastSyncAt: nil, now: now))
        XCTAssertFalse(AutoSync.isDue(lastSyncAt: now.addingTimeInterval(-60), now: now))
        XCTAssertTrue(AutoSync.isDue(lastSyncAt: now.addingTimeInterval(-15 * 60), now: now))
        XCTAssertTrue(AutoSync.isDue(lastSyncAt: now.addingTimeInterval(-(15 * 60 - 2)), now: now))
    }
}

final class CLITests: XCTestCase {
    func testLocatorOrder() {
        let home = "/Users/alice"
        XCTAssertEqual(CLILocator.defaultPath(home: home, isExecutable: { _ in true }), "/Users/alice/bin/livingbrain")
        XCTAssertEqual(CLILocator.defaultPath(home: home, isExecutable: { $0.hasPrefix("/usr/local") }), "/usr/local/bin/livingbrain")
        XCTAssertNil(CLILocator.defaultPath(home: home, isExecutable: { _ in false }))
    }

    func testLocatorPrefersConfiguredWhenExecutable() {
        let exists: (String) -> Bool = { $0 == "/Users/alice/tools/lb" || $0 == "/opt/homebrew/bin/livingbrain" }
        XCTAssertEqual(CLILocator.resolve(configured: "/Users/alice/tools/lb", home: "/Users/alice", isExecutable: exists),
                       "/Users/alice/tools/lb")
        XCTAssertEqual(CLILocator.resolve(configured: "/gone", home: "/Users/alice", isExecutable: exists),
                       "/opt/homebrew/bin/livingbrain")
    }

    func testEnvironmentSetsAPIURLOnlyWhenUnset() {
        let env = CLIEnvironment.make(base: ["PATH": "/usr/bin"])
        XCTAssertEqual(env["LIVINGBRAIN_API_URL"], "https://app.livingbrain.wiki")
        XCTAssertTrue(env["PATH"]!.hasPrefix("/usr/bin:"))
        XCTAssertTrue(env["PATH"]!.contains("/opt/homebrew/bin"))
        let kept = CLIEnvironment.make(base: ["LIVINGBRAIN_API_URL": "http://localhost:8787"])
        XCTAssertEqual(kept["LIVINGBRAIN_API_URL"], "http://localhost:8787")
    }

    func testRunnerCollectsOutputAndStatus() async throws {
        let lines = LineBox()
        let result = try await CLIRunner.run(
            executable: "/bin/sh",
            arguments: ["-c", "echo '{\"ok\":1}'; echo first >&2; echo second >&2; exit 4"],
            environment: [:],
            onStderrLine: { lines.append($0) }
        )
        XCTAssertEqual(result.status, 4)
        XCTAssertEqual(String(decoding: result.stdout, as: UTF8.self), "{\"ok\":1}\n")
        XCTAssertEqual(result.stderr, "first\nsecond\n")
        XCTAssertEqual(lines.all, ["first", "second"])
    }

    func testRunnerReportsMissingBinary() async {
        do {
            _ = try await CLIRunner.run(executable: "/nonexistent/livingbrain", arguments: [], environment: [:])
            XCTFail("expected a launch error")
        } catch {
            XCTAssertTrue(error is CLIRunError)
        }
    }

    func testAllowlistLocation() {
        XCTAssertEqual(AllowlistLocation.path(environment: [:], home: "/Users/alice"),
                       "/Users/alice/.config/livingbrain/logs-allow.json")
        XCTAssertEqual(AllowlistLocation.path(environment: ["XDG_CONFIG_HOME": "/cfg"], home: "/Users/alice"),
                       "/cfg/livingbrain/logs-allow.json")
        XCTAssertEqual(SyncedProject(path: "/Users/alice/code/atlas").displayPath(home: "/Users/alice"), "~/code/atlas")
        XCTAssertEqual(SyncedProject(path: "/Users/alice/code/atlas").name, "atlas")
        XCTAssertEqual(SyncedProject(path: "/Users/alicex").displayPath(home: "/Users/alice"), "/Users/alicex")
    }
}

final class LineBox: @unchecked Sendable {
    private let lock = NSLock()
    private var lines: [String] = []
    func append(_ line: String) { lock.lock(); lines.append(line); lock.unlock() }
    var all: [String] { lock.lock(); defer { lock.unlock() }; return lines }
}
