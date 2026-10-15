import Foundation

// The local runner: "run brain jobs with my subscription".
//
// Anthropic and OpenAI only allow a Claude or ChatGPT subscription inside
// their own clients, so the brain never holds those credentials. Instead the
// user's own `claude` / `codex` CLI on this Mac makes the model call. The app
// does not build that call itself: like sync, it is a thin shell over the
// `livingbrain` CLI, which owns the safety-critical flags (no tools, read-only
// sandbox, never a permission bypass) in one place:
//
//   livingbrain --json jobs agents              which agents are installed and signed in
//   livingbrain --json jobs run --agent claude  claim, run and answer jobs until stopped
//
// `jobs run` prints one JSON object per line (`RunnerEvent`). Prompts and
// model output never appear in it, and this app never logs either.

/// The coding agent whose subscription runs the jobs.
public enum RunnerAgent: String, CaseIterable, Identifiable, Sendable, Codable {
    case claude, codex

    public var id: String { rawValue }

    public var title: String {
        switch self {
        case .claude: return "Claude Code"
        case .codex: return "Codex"
        }
    }

    public var subscription: String {
        switch self {
        case .claude: return "Claude subscription"
        case .codex: return "ChatGPT subscription"
        }
    }

    /// The models offered in the picker. Claude Code takes an alias for the
    /// latest model (`claude --help`: "e.g. 'fable', 'opus', or 'sonnet'");
    /// Codex takes a model slug (`codex exec -m`). Empty means the CLI's own
    /// default. Any other id can be typed as a custom model.
    public var models: [String] {
        switch self {
        case .claude: return ["sonnet", "opus", "haiku", "fable"]
        case .codex: return ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4-mini"]
        }
    }

    public var defaultModel: String { models[0] }
}

/// The `livingbrain` arguments the runner uses. Kept here, and tested, so no
/// flag that would widen what a job may do can creep in.
public enum JobsCommand {
    public static func agents() -> [String] {
        ["--json", "jobs", "agents"]
    }

    /// `--agent` pins every job to this agent (a job names its own otherwise).
    public static func run(agent: RunnerAgent, once: Bool = false) -> [String] {
        var arguments = ["--json", "jobs", "run", "--agent", agent.rawValue]
        if once { arguments.append("--once") }
        return arguments
    }

    /// Words that must never reach a runner's argv: a permission bypass or a
    /// write-anywhere sandbox would let a job do more than produce text.
    public static let forbidden = ["dangerously", "skip-permissions", "bypass", "danger-full-access"]

    public static func isSafe(_ arguments: [String]) -> Bool {
        !arguments.contains { argument in
            let lower = argument.lowercased()
            return forbidden.contains { lower.contains($0) }
        }
    }
}

/// One agent in `livingbrain jobs agents --json`. No email or org: the CLI
/// reports only whether the agent is signed in and how.
public struct AgentStatus: Decodable, Equatable, Sendable, Identifiable {
    public var agent: String
    public var found: Bool
    public var path: String?
    public var version: String?
    public var loggedIn: Bool
    /// e.g. "claude.ai (max)" or "ChatGPT".
    public var login: String?

    public var id: String { agent }
    public var runnerAgent: RunnerAgent? { RunnerAgent(rawValue: agent) }

    enum CodingKeys: String, CodingKey {
        case agent, found, path, version, login
        case loggedIn = "logged_in"
    }

    public init(agent: String, found: Bool, path: String? = nil, version: String? = nil,
                loggedIn: Bool = false, login: String? = nil)
    {
        self.agent = agent
        self.found = found
        self.path = path
        self.version = version
        self.loggedIn = loggedIn
        self.login = login
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        agent = try c.decode(String.self, forKey: .agent)
        path = try c.decodeIfPresent(String.self, forKey: .path)
        found = try c.decodeIfPresent(Bool.self, forKey: .found) ?? (path != nil)
        version = try c.decodeIfPresent(String.self, forKey: .version)
        loggedIn = try c.decodeIfPresent(Bool.self, forKey: .loggedIn) ?? false
        login = try c.decodeIfPresent(String.self, forKey: .login)
    }

    /// "Signed in · claude.ai (max)", "Not signed in", "Not installed".
    public var summary: String {
        guard found else { return "Not installed" }
        guard loggedIn else { return "Not signed in" }
        return ["Signed in", login].compactMap { $0?.isEmpty == false ? $0 : nil }.joined(separator: " · ")
    }
}

public struct AgentsReport: Decodable, Equatable, Sendable {
    public var agents: [AgentStatus]

    public func status(_ agent: RunnerAgent) -> AgentStatus? {
        agents.first { $0.agent == agent.rawValue }
    }

    public static func parse(_ data: Data) throws -> AgentsReport {
        try JSONDecoder().decode(AgentsReport.self, from: data)
    }
}

/// One line of `livingbrain jobs run --json`.
public enum RunnerEvent: Equatable, Sendable {
    case started(agents: [String])
    case claimed(job: String, kind: String, agent: String?, model: String?)
    case done(job: String, kind: String, seconds: Double?, promptTokens: Int, completionTokens: Int)
    case failed(job: String, reason: String)
    case error(String)
    case stopped
    /// A line this app does not know (a newer CLI): ignored.
    case other(String)

    /// Parses one stdout line; nil for a blank or non-JSON line.
    public static func parse(_ line: String) -> RunnerEvent? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty,
              let object = (try? JSONSerialization.jsonObject(with: Data(trimmed.utf8))) as? [String: Any],
              let event = object["event"] as? String
        else { return nil }
        func string(_ key: String) -> String? { object[key] as? String }
        func int(_ key: String) -> Int { (object[key] as? NSNumber)?.intValue ?? 0 }
        switch event {
        case "started":
            return .started(agents: object["agents"] as? [String] ?? [])
        case "claimed":
            return .claimed(job: string("job") ?? "", kind: string("kind") ?? "job",
                            agent: string("agent"), model: string("model"))
        case "done":
            return .done(job: string("job") ?? "", kind: string("kind") ?? "job",
                         seconds: (object["seconds"] as? NSNumber)?.doubleValue,
                         promptTokens: int("prompt_tokens"), completionTokens: int("completion_tokens"))
        case "failed":
            return .failed(job: string("job") ?? "", reason: string("reason") ?? "failed")
        case "error":
            return .error(Scrub.text(string("message") ?? "runner error"))
        case "stopped":
            return .stopped
        default:
            return .other(event)
        }
    }
}

/// Splits a byte stream into lines; a partial line waits for its newline.
public struct LineSplitter: Sendable {
    private var pending = Data()

    public init() {}

    public mutating func feed(_ data: Data) -> [String] {
        pending.append(data)
        var lines: [String] = []
        while let newline = pending.firstIndex(of: 0x0A) {
            lines.append(String(decoding: pending[pending.startIndex..<newline], as: UTF8.self))
            pending.removeSubrange(pending.startIndex...newline)
        }
        return lines
    }

    public mutating func finish() -> [String] {
        defer { pending = Data() }
        return pending.isEmpty ? [] : [String(decoding: pending, as: UTF8.self)]
    }
}

/// What the menu says about the runner, built from its events.
public struct RunnerActivity: Equatable, Sendable {
    public struct Current: Equatable, Sendable {
        public var job: String
        public var kind: String
        public var model: String?
        public var since: Date
    }

    public struct Finished: Equatable, Sendable {
        public var kind: String
        public var at: Date
        public var tokens: Int
    }

    public var running = false
    public var current: Current?
    public var lastDone: Finished?
    public var lastError: String?
    public var completed = 0
    public var failed = 0

    public init() {}

    public mutating func apply(_ event: RunnerEvent, at now: Date) {
        switch event {
        case .started:
            running = true
            lastError = nil
        case let .claimed(job, kind, _, model):
            current = Current(job: job, kind: kind, model: model, since: now)
        case let .done(_, kind, _, prompt, completion):
            current = nil
            completed += 1
            lastDone = Finished(kind: kind, at: now, tokens: prompt + completion)
            lastError = nil
        case let .failed(_, reason):
            current = nil
            failed += 1
            lastError = "Last job failed: \(reason)"
        case let .error(message):
            lastError = message
        case .stopped:
            running = false
            current = nil
        case .other:
            break
        }
    }

    /// The process exited (or could not start).
    public mutating func exited(message: String?) {
        running = false
        current = nil
        if let message { lastError = message }
    }

    /// The menu's lines, e.g. "Running a compile job with Claude Code…" and
    /// "Last job done 2 min ago · 1,234 tokens".
    public func lines(agent: RunnerAgent, now: Date) -> [String] {
        var out: [String] = []
        if let current {
            out.append("Running \(Self.article(current.kind)) \(current.kind) job with \(agent.title)…")
        } else if running {
            out.append("Waiting for jobs (\(agent.title))")
        } else {
            out.append("Runner stopped")
        }
        if let lastDone {
            let tokens = lastDone.tokens.formatted(.number)
            out.append("Last job done \(Self.ago(lastDone.at, now: now)) · \(tokens) tokens")
        }
        if let lastError { out.append("⚠︎ " + lastError) }
        return out
    }

    static func article(_ word: String) -> String {
        guard let first = word.lowercased().first else { return "a" }
        return "aeiou".contains(first) ? "an" : "a"
    }

    static func ago(_ date: Date, now: Date) -> String {
        let seconds = max(0, Int(now.timeIntervalSince(date)))
        if seconds < 60 { return "just now" }
        if seconds < 3600 { return "\(seconds / 60) min ago" }
        if seconds < 86400 { return "\(seconds / 3600) h ago" }
        return "\(seconds / 86400) d ago"
    }
}

/// Restart delay after the runner exits on its own: 5 s, doubling, capped at
/// 5 min; a run that lasted a while starts the count again.
public struct RunnerBackoff: Equatable, Sendable {
    public private(set) var failures = 0

    public init() {}

    public mutating func next(ranFor seconds: TimeInterval) -> TimeInterval {
        if seconds > 120 { failures = 0 }
        failures += 1
        return min(5 * pow(2, Double(failures - 1)), 300)
    }

    public mutating func reset() { failures = 0 }
}

/// This user's runner connection (`GET /v1/models/mine`): jobs owned by this
/// user, for that role, run on their device.
public struct RunnerConnection: Decodable, Equatable, Sendable, Identifiable {
    public var role: String
    public var kind: String
    public var agent: String
    public var model: String
    public var settings: String?
    public var updatedAt: String?
    public var lastSeenAt: String?

    public var id: String { role }

    enum CodingKeys: String, CodingKey {
        case role, kind, agent, model, settings
        case updatedAt = "updated_at"
        case lastSeenAt = "last_seen_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        role = try c.decode(String.self, forKey: .role)
        kind = try c.decodeIfPresent(String.self, forKey: .kind) ?? "local_runner"
        agent = try c.decodeIfPresent(String.self, forKey: .agent) ?? ""
        model = try c.decodeIfPresent(String.self, forKey: .model) ?? ""
        settings = try c.decodeIfPresent(String.self, forKey: .settings)
        updatedAt = try c.decodeIfPresent(String.self, forKey: .updatedAt)
        lastSeenAt = try c.decodeIfPresent(String.self, forKey: .lastSeenAt)
    }
}

struct RunnersEnvelope: Decodable { var runners: [RunnerConnection] }

public extension APIClient {
    /// `GET /v1/models/mine`.
    func myRunners() async throws -> [RunnerConnection] {
        try await get(RunnersEnvelope.self, "/v1/models/mine").runners
    }

    /// `PUT /v1/models/mine/{role}`: route this user's jobs for `role` to
    /// their own device. Any member may; it spends no workspace key.
    func setRunner(role: ModelRole = .main, agent: RunnerAgent, model: String) async throws -> RunnerConnection {
        struct Body: Encodable { var kind: String; var agent: String; var model: String; var settings: String }
        let request = try request("PUT", "/v1/models/mine/\(role.rawValue)", json: Body(
            kind: "local_runner", agent: agent.rawValue,
            model: model.trimmingCharacters(in: .whitespacesAndNewlines), settings: "isolated"
        ))
        return try decode(RunnerConnection.self, await send(request))
    }

    /// `DELETE /v1/models/mine/{role}`: this user's jobs go back to the
    /// workspace's own connection.
    func removeRunner(role: ModelRole = .main) async throws {
        _ = try await send(request("DELETE", "/v1/models/mine/\(role.rawValue)"))
    }
}

/// The long-running `livingbrain jobs run` child: stdout lines go to
/// `onLine`, and `onExit` fires once with the exit status. Stderr is kept
/// only as its last line (a CLI error message), never logged.
public final class RunnerProcess: @unchecked Sendable {
    private let process = Process()
    private let lock = NSLock()
    private var splitter = LineSplitter()
    private var lastStderr: String?

    public init() {}

    public var isRunning: Bool { process.isRunning }

    public func start(
        executable: String,
        arguments: [String],
        environment: [String: String],
        onLine: @escaping @Sendable (String) -> Void,
        onExit: @escaping @Sendable (Int32, String?) -> Void
    ) throws {
        guard JobsCommand.isSafe(arguments) else {
            throw CLIRunError.launch("refusing an unsafe runner command")
        }
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = environment
        process.currentDirectoryURL = URL(fileURLWithPath: NSHomeDirectory())
        process.standardInput = FileHandle.nullDevice
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        out.fileHandleForReading.readabilityHandler = { [weak self] handle in
            guard let self else { return }
            let data = handle.availableData
            self.lock.lock()
            let lines = self.splitter.feed(data)
            self.lock.unlock()
            lines.forEach(onLine)
        }
        err.fileHandleForReading.readabilityHandler = { [weak self] handle in
            guard let self else { return }
            let text = String(decoding: handle.availableData, as: UTF8.self)
            let last = text.split(separator: "\n").last.map(String.init)
            if let last, !last.isEmpty {
                self.lock.lock(); self.lastStderr = last; self.lock.unlock()
            }
        }
        process.terminationHandler = { [weak self] process in
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            guard let self else { return }
            self.lock.lock()
            var lines = self.splitter.feed(out.fileHandleForReading.readDataToEndOfFile())
            lines += self.splitter.finish()
            let message = self.lastStderr.map { Scrub.text(CLIOutput.firstErrorLine($0) ?? $0) }
            self.lock.unlock()
            lines.forEach(onLine)
            onExit(process.terminationStatus, message)
        }
        do {
            try process.run()
        } catch {
            process.terminationHandler = nil
            out.fileHandleForReading.readabilityHandler = nil
            err.fileHandleForReading.readabilityHandler = nil
            throw CLIRunError.launch(error.localizedDescription)
        }
    }

    /// SIGTERM: the CLI finishes or abandons its job (the lease lapses on the
    /// server) and prints `stopped`.
    public func stop() {
        if process.isRunning { process.terminate() }
    }
}
