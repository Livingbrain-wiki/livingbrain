import Foundation
import XCTest
@testable import LivingBrainKit

final class JobRunnerTests: XCTestCase {
    // MARK: Command construction

    func testRunCommandPinsTheAgentAndAsksForJSON() {
        XCTAssertEqual(JobsCommand.run(agent: .claude), ["--json", "jobs", "run", "--agent", "claude"])
        XCTAssertEqual(JobsCommand.run(agent: .codex, once: true),
                       ["--json", "jobs", "run", "--agent", "codex", "--once"])
        XCTAssertEqual(JobsCommand.agents(), ["--json", "jobs", "agents"])
    }

    func testNoCommandCarriesAPermissionBypass() {
        for agent in RunnerAgent.allCases {
            for arguments in [JobsCommand.run(agent: agent), JobsCommand.run(agent: agent, once: true), JobsCommand.agents()] {
                XCTAssertTrue(JobsCommand.isSafe(arguments), "\(arguments)")
                let joined = arguments.joined(separator: " ").lowercased()
                for word in ["--dangerously-skip-permissions", "skip-permissions", "bypass", "danger"] {
                    XCTAssertFalse(joined.contains(word), "\(word) in \(arguments)")
                }
            }
        }
    }

    func testTheSafetyCheckRefusesBypassFlags() {
        for bad in [
            ["jobs", "run", "--dangerously-skip-permissions"],
            ["--permission-mode", "bypassPermissions"],
            ["--sandbox", "danger-full-access"],
            ["--dangerously-bypass-approvals-and-sandbox"],
        ] {
            XCTAssertFalse(JobsCommand.isSafe(bad), "\(bad)")
        }
    }

    func testTheProcessRefusesToStartAnUnsafeCommand() {
        let process = RunnerProcess()
        XCTAssertThrowsError(try process.start(
            executable: "/usr/bin/true", arguments: ["--dangerously-skip-permissions"], environment: [:],
            onLine: { _ in }, onExit: { _, _ in }
        ))
        XCTAssertFalse(process.isRunning)
    }

    func testTheProcessStreamsEventsFromAFakeCLI() throws {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("runner-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let script = dir.appendingPathComponent("livingbrain")
        try #"""
        #!/bin/sh
        [ "$*" = "--json jobs run --agent claude --once" ] || { echo "error: unexpected args: $*" >&2; exit 3; }
        printf '{"event":"started","agents":["claude"]}\n{"event":"claimed","job":"j1","kind":"compile","agent":"claude","model":"sonnet"}\n'
        printf '{"event":"done","job":"j1","kind":"compile","seconds":1,"prompt_tokens":5,"completion_tokens":7}\n{"event":"stopped"}'
        """#.write(to: script, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)

        let lines = LockedLines()
        let exited = expectation(description: "exit")
        let status = LockedLines()
        let process = RunnerProcess()
        try process.start(
            executable: script.path, arguments: JobsCommand.run(agent: .claude, once: true), environment: [:],
            onLine: { lines.append($0) },
            onExit: { code, _ in status.append(String(code)); exited.fulfill() }
        )
        wait(for: [exited], timeout: 10)
        XCTAssertEqual(status.all, ["0"])
        var activity = RunnerActivity()
        for line in lines.all { if let event = RunnerEvent.parse(line) { activity.apply(event, at: Date()) } }
        XCTAssertEqual(activity.completed, 1)
        XCTAssertEqual(activity.lastDone?.tokens, 12)
        XCTAssertFalse(activity.running, "the trailing `stopped` without a newline is still read")
    }

    func testModelsPerAgent() {
        XCTAssertEqual(RunnerAgent.claude.models, ["sonnet", "opus", "haiku", "fable"])
        XCTAssertEqual(RunnerAgent.codex.defaultModel, "gpt-5.6-sol")
        XCTAssertTrue(RunnerAgent.codex.models.contains("gpt-5.4-mini"))
    }

    // MARK: Agents report

    func testAgentsReportParses() throws {
        let json = #"""
        {"agents":[
          {"agent":"claude","found":true,"path":"/Users/me/.local/bin/claude","version":"2.1.300","logged_in":true,"login":"claude.ai (max)"},
          {"agent":"codex","found":false,"path":null,"version":null,"logged_in":false,"login":null,"future":1}
        ]}
        """#
        let report = try AgentsReport.parse(Data(json.utf8))
        let claude = try XCTUnwrap(report.status(.claude))
        XCTAssertTrue(claude.found)
        XCTAssertTrue(claude.loggedIn)
        XCTAssertEqual(claude.summary, "Signed in · claude.ai (max)")
        XCTAssertEqual(claude.runnerAgent, .claude)
        let codex = try XCTUnwrap(report.status(.codex))
        XCTAssertEqual(codex.summary, "Not installed")
        XCTAssertEqual(AgentStatus(agent: "codex", found: true).summary, "Not signed in")
    }

    // MARK: Events

    func testEventsParse() {
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"started","agents":["claude","codex"]}"#),
                       .started(agents: ["claude", "codex"]))
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"claimed","job":"j1","kind":"compile","agent":"claude","model":"sonnet"}"#),
                       .claimed(job: "j1", kind: "compile", agent: "claude", model: "sonnet"))
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"done","job":"j1","kind":"compile","seconds":12.5,"prompt_tokens":1000,"completion_tokens":234}"#),
                       .done(job: "j1", kind: "compile", seconds: 12.5, promptTokens: 1000, completionTokens: 234))
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"failed","job":"j2","reason":"timeout"}"#),
                       .failed(job: "j2", reason: "timeout"))
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"stopped"}"#), .stopped)
        XCTAssertEqual(RunnerEvent.parse(#"{"event":"heartbeat"}"#), .other("heartbeat"))
        XCTAssertNil(RunnerEvent.parse(""))
        XCTAssertNil(RunnerEvent.parse("not json"))
        XCTAssertNil(RunnerEvent.parse(#"{"no":"event"}"#))
    }

    func testAnErrorMessageIsScrubbed() {
        guard case let .error(message)? = RunnerEvent.parse(#"{"event":"error","message":"HTTP 500 Bearer abcdefghijklmnop"}"#) else {
            return XCTFail("not an error event")
        }
        XCTAssertFalse(message.contains("abcdefghijklmnop"))
    }

    func testLinesSplitAcrossChunks() {
        var splitter = LineSplitter()
        XCTAssertEqual(splitter.feed(Data(#"{"event":"sta"#.utf8)), [])
        XCTAssertEqual(splitter.feed(Data("rted\"}\n{\"event\":\"stopped\"}\n{\"ev".utf8)),
                       [#"{"event":"started"}"#, #"{"event":"stopped"}"#])
        XCTAssertEqual(splitter.finish(), [#"{"ev"#])
        XCTAssertEqual(splitter.finish(), [])
    }

    // MARK: Activity

    func testActivityTellsTheMenuWhatIsHappening() {
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        var activity = RunnerActivity()
        XCTAssertEqual(activity.lines(agent: .claude, now: t0), ["Runner stopped"])
        activity.apply(.started(agents: ["claude"]), at: t0)
        XCTAssertEqual(activity.lines(agent: .claude, now: t0), ["Waiting for jobs (Claude Code)"])
        activity.apply(.claimed(job: "j1", kind: "compile", agent: "claude", model: "sonnet"), at: t0)
        XCTAssertEqual(activity.lines(agent: .claude, now: t0), ["Running a compile job with Claude Code…"])
        activity.apply(.done(job: "j1", kind: "compile", seconds: 3, promptTokens: 1000, completionTokens: 234), at: t0)
        let lines = activity.lines(agent: .claude, now: t0.addingTimeInterval(125))
        XCTAssertEqual(lines.count, 2)
        XCTAssertEqual(lines[0], "Waiting for jobs (Claude Code)")
        XCTAssertTrue(lines[1].hasPrefix("Last job done 2 min ago · "), lines[1])
        XCTAssertTrue(lines[1].hasSuffix(" tokens"))
        XCTAssertEqual(activity.completed, 1)
        activity.apply(.claimed(job: "j2", kind: "ask", agent: nil, model: nil), at: t0)
        XCTAssertEqual(activity.lines(agent: .codex, now: t0).first, "Running an ask job with Codex…")
        activity.apply(.failed(job: "j2", reason: "timeout"), at: t0)
        XCTAssertEqual(activity.failed, 1)
        XCTAssertEqual(activity.lines(agent: .codex, now: t0).last, "⚠︎ Last job failed: timeout")
        activity.exited(message: "boom")
        XCTAssertFalse(activity.running)
        XCTAssertEqual(activity.lines(agent: .codex, now: t0).first, "Runner stopped")
    }

    func testBackoffDoublesAndResetsAfterALongRun() {
        var backoff = RunnerBackoff()
        XCTAssertEqual(backoff.next(ranFor: 1), 5)
        XCTAssertEqual(backoff.next(ranFor: 1), 10)
        XCTAssertEqual(backoff.next(ranFor: 1), 20)
        for _ in 0..<10 { _ = backoff.next(ranFor: 1) }
        XCTAssertEqual(backoff.next(ranFor: 1), 300)
        XCTAssertEqual(backoff.next(ranFor: 600), 5)
    }

    // MARK: API

    func testSetRunnerPutsTheLocalRunnerForMain() async throws {
        let mock = MockTransport()
        let client = APIClient(baseURL: URL(string: "https://app.livingbrain.wiki")!, token: "tok-secret-123", transport: mock)
        mock.on("PUT", "/v1/models/mine/main", Data(#"""
        {"role":"main","kind":"local_runner","agent":"codex","model":"gpt-5.5","settings":"isolated","updated_at":"2026-10-15T00:00:00Z","last_seen_at":null}
        """#.utf8))
        let row = try await client.setRunner(role: .main, agent: .codex, model: " gpt-5.5 ")
        XCTAssertEqual(row.agent, "codex")
        XCTAssertEqual(row.model, "gpt-5.5")
        XCTAssertNil(row.lastSeenAt)
        let request = try XCTUnwrap(mock.last)
        XCTAssertEqual(request.httpMethod, "PUT")
        XCTAssertEqual(request.url?.absoluteString, "https://app.livingbrain.wiki/v1/models/mine/main")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer tok-secret-123")
        let body = try mock.lastJSON()
        XCTAssertEqual(body["kind"] as? String, "local_runner")
        XCTAssertEqual(body["agent"] as? String, "codex")
        XCTAssertEqual(body["model"] as? String, "gpt-5.5")
        XCTAssertEqual(body["settings"] as? String, "isolated")
        XCTAssertEqual(Set(body.keys), ["kind", "agent", "model", "settings"], "no credential of any kind is sent")
    }

    func testRemoveRunnerDeletesAndMyRunnersLists() async throws {
        let mock = MockTransport()
        let client = APIClient(baseURL: URL(string: "https://app.livingbrain.wiki")!, token: "tok", transport: mock)
        mock.on("DELETE", "/v1/models/mine/main", status: 204, Data())
        try await client.removeRunner()
        XCTAssertEqual(mock.last?.httpMethod, "DELETE")
        XCTAssertEqual(mock.last?.url?.path, "/v1/models/mine/main")
        XCTAssertNil(mock.last?.httpBody)

        mock.on("GET", "/v1/models/mine", Data(#"{"runners":[{"role":"main","kind":"local_runner","agent":"claude","model":"sonnet","last_seen_at":"2026-10-15T01:02:03Z"}]}"#.utf8))
        let runners = try await client.myRunners()
        XCTAssertEqual(runners.map(\.agent), ["claude"])
        XCTAssertEqual(runners.first?.lastSeenAt, "2026-10-15T01:02:03Z")
    }
}

final class LockedLines: @unchecked Sendable {
    private let lock = NSLock()
    private var lines: [String] = []
    func append(_ line: String) { lock.lock(); lines.append(line); lock.unlock() }
    var all: [String] { lock.lock(); defer { lock.unlock() }; return lines }
}
