import Foundation

// The shapes `livingbrain --json …` prints. Each mirrors a `json!` literal in
// crates/livingbrain-cli/src; unknown fields are ignored so a newer CLI does
// not break an older menu bar app.

/// `livingbrain logs sync --json` (crates/livingbrain-cli/src/logs.rs, `report`).
public struct SyncReport: Decodable, Equatable, Sendable {
    public var scanned: Int
    public var uploaded: Int
    public var unchanged: Int
    public var skipped: Int
    public var unreadable: Int
    public var unparsed: Int
    public var redacted: Int
    public var dryRun: Bool
    /// Note outcomes by status: `written`, `unchanged`, `failed`,
    /// `would_write`, `not_written` (logs/note.rs, `report`).
    public var notes: [String: Int]
    public var sessions: [SyncedSession]

    enum CodingKeys: String, CodingKey {
        case scanned, uploaded, unchanged, skipped, unreadable, unparsed, redacted, notes, sessions
        case dryRun = "dry_run"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        scanned = try c.decodeIfPresent(Int.self, forKey: .scanned) ?? 0
        uploaded = try c.decodeIfPresent(Int.self, forKey: .uploaded) ?? 0
        unchanged = try c.decodeIfPresent(Int.self, forKey: .unchanged) ?? 0
        skipped = try c.decodeIfPresent(Int.self, forKey: .skipped) ?? 0
        unreadable = try c.decodeIfPresent(Int.self, forKey: .unreadable) ?? 0
        unparsed = try c.decodeIfPresent(Int.self, forKey: .unparsed) ?? 0
        redacted = try c.decodeIfPresent(Int.self, forKey: .redacted) ?? 0
        dryRun = try c.decodeIfPresent(Bool.self, forKey: .dryRun) ?? false
        notes = try c.decodeIfPresent([String: Int].self, forKey: .notes) ?? [:]
        sessions = try c.decodeIfPresent([SyncedSession].self, forKey: .sessions) ?? []
    }

    public init(
        scanned: Int = 0, uploaded: Int = 0, unchanged: Int = 0, skipped: Int = 0,
        unreadable: Int = 0, unparsed: Int = 0, redacted: Int = 0, dryRun: Bool = false,
        notes: [String: Int] = [:], sessions: [SyncedSession] = []
    ) {
        self.scanned = scanned
        self.uploaded = uploaded
        self.unchanged = unchanged
        self.skipped = skipped
        self.unreadable = unreadable
        self.unparsed = unparsed
        self.redacted = redacted
        self.dryRun = dryRun
        self.notes = notes
        self.sessions = sessions
    }

    /// Notes the sync wrote (or, on a dry run, would write).
    public var notesWritten: Int {
        (notes["written"] ?? 0) + (notes["would_write"] ?? 0)
    }

    /// One line for the menu: "2 uploaded · 10 unchanged · 700 skipped · 2 notes written".
    public var summary: String {
        var parts: [String] = []
        if dryRun {
            parts.append("\(sessions.filter { $0.status == "would_upload" }.count) would upload")
        } else {
            parts.append("\(uploaded) uploaded")
            parts.append("\(unchanged) unchanged")
        }
        parts.append("\(skipped) skipped")
        parts.append("\(notesWritten) note\(notesWritten == 1 ? "" : "s") written")
        return parts.joined(separator: " · ")
    }
}

/// One entry of `SyncReport.sessions`.
public struct SyncedSession: Decodable, Equatable, Sendable {
    public var agent: String
    public var sessionId: String
    public var repo: String
    public var models: [String]
    public var turns: Int
    public var toolCalls: Int
    public var truncated: Bool
    /// `uploaded`, `unchanged`, `skipped` or `would_upload`.
    public var status: String
    public var id: String?
    public var totals: Totals?
    /// Added with `logs sync --session`; absent from older CLIs.
    public var branch: String?
    public var startedAt: String?
    public var endedAt: String?
    /// The local log file (never sent).
    public var file: String?
    public var note: Note?

    /// The searchable note a synced session gets (logs/note.rs).
    public struct Note: Decodable, Equatable, Sendable {
        public var slug: String?
        public var project: String?
        /// `written`, `unchanged`, `failed`, `would_write`, `not_written`.
        public var status: String?
        public var url: String?
        public var error: String?
    }

    /// Stable across runs: one session of one agent.
    public var key: String { "\(agent)/\(sessionId)" }

    public var isOptedIn: Bool { status != "skipped" }

    /// Every token the session's own log counted.
    public var totalTokens: Int {
        guard let t = totals else { return 0 }
        return t.inputTokens + t.outputTokens + t.cacheReadTokens + t.cacheCreationTokens + t.reasoningTokens
    }

    public var startedDate: Date? { startedAt.flatMap(ISODate.parse) }
    public var endedDate: Date? { endedAt.flatMap(ISODate.parse) }

    /// The repo's folder name: what the server files the session under.
    public var projectName: String {
        let base = (repo as NSString).lastPathComponent
        return base == "~" || base.isEmpty ? "(home)" : base
    }

    public struct Totals: Decodable, Equatable, Sendable {
        public var inputTokens: Int
        public var outputTokens: Int
        public var cacheReadTokens: Int
        public var cacheCreationTokens: Int
        public var reasoningTokens: Int
        public var costUsd: Double?

        enum CodingKeys: String, CodingKey {
            case inputTokens = "input_tokens"
            case outputTokens = "output_tokens"
            case cacheReadTokens = "cache_read_tokens"
            case cacheCreationTokens = "cache_creation_tokens"
            case reasoningTokens = "reasoning_tokens"
            case costUsd = "cost_usd"
        }

        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            inputTokens = try c.decodeIfPresent(Int.self, forKey: .inputTokens) ?? 0
            outputTokens = try c.decodeIfPresent(Int.self, forKey: .outputTokens) ?? 0
            cacheReadTokens = try c.decodeIfPresent(Int.self, forKey: .cacheReadTokens) ?? 0
            cacheCreationTokens = try c.decodeIfPresent(Int.self, forKey: .cacheCreationTokens) ?? 0
            reasoningTokens = try c.decodeIfPresent(Int.self, forKey: .reasoningTokens) ?? 0
            costUsd = try c.decodeIfPresent(Double.self, forKey: .costUsd)
        }
    }

    enum CodingKeys: String, CodingKey {
        case agent, repo, models, turns, truncated, status, id, totals, branch, file, note
        case sessionId = "session_id"
        case toolCalls = "tool_calls"
        case startedAt = "started_at"
        case endedAt = "ended_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        agent = try c.decodeIfPresent(String.self, forKey: .agent) ?? ""
        sessionId = try c.decodeIfPresent(String.self, forKey: .sessionId) ?? ""
        repo = try c.decodeIfPresent(String.self, forKey: .repo) ?? ""
        models = try c.decodeIfPresent([String].self, forKey: .models) ?? []
        turns = try c.decodeIfPresent(Int.self, forKey: .turns) ?? 0
        toolCalls = try c.decodeIfPresent(Int.self, forKey: .toolCalls) ?? 0
        truncated = try c.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
        status = try c.decodeIfPresent(String.self, forKey: .status) ?? ""
        id = try c.decodeIfPresent(String.self, forKey: .id)
        totals = try c.decodeIfPresent(Totals.self, forKey: .totals)
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        startedAt = try c.decodeIfPresent(String.self, forKey: .startedAt)
        endedAt = try c.decodeIfPresent(String.self, forKey: .endedAt)
        file = try c.decodeIfPresent(String.self, forKey: .file)
        note = try c.decodeIfPresent(Note.self, forKey: .note)
    }
}

/// `livingbrain logs status --json`, and the `allowed` half of
/// `logs allow|deny --json`.
public struct AllowStatus: Decodable, Equatable, Sendable {
    public var allowed: [String]
    /// The opt-in file's path.
    public var config: String?
    /// Only on `allow`/`deny`.
    public var changed: Bool?

    public init(allowed: [String], config: String? = nil, changed: Bool? = nil) {
        self.allowed = allowed
        self.config = config
        self.changed = changed
    }
}

/// The opt-in file itself, `~/.config/livingbrain/logs-allow.json`
/// (logs.rs, `AllowlistFile`): `{"version":1,"allowed":["/abs/path", …]}`.
public struct AllowlistFile: Decodable, Equatable, Sendable {
    public static let supportedVersion = 1
    public var version: Int
    public var allowed: [String]
}

/// The device-flow prompt `livingbrain --json login` writes to stderr
/// (auth.rs, `login`). Contains no token: the token only ever reaches the
/// keychain.
public struct LoginPrompt: Decodable, Equatable, Sendable {
    public var userCode: String
    public var verificationUri: String
    public var verificationUriComplete: String?

    enum CodingKeys: String, CodingKey {
        case userCode = "user_code"
        case verificationUri = "verification_uri"
        case verificationUriComplete = "verification_uri_complete"
    }

    /// The page to open: the complete URI pre-fills the code.
    public var openURL: URL? {
        URL(string: verificationUriComplete ?? verificationUri)
    }
}

public enum CLIOutputError: Error, Equatable {
    case empty
    case unsupportedAllowlistVersion(Int)
}

public enum CLIOutput {
    static let decoder = JSONDecoder()

    /// The last non-empty line of stdout: `--json` prints exactly one JSON
    /// value, but a wrapper script may print before it.
    static func lastJSONLine(_ data: Data) throws -> Data {
        let text = String(decoding: data, as: UTF8.self)
        guard let line = text.split(whereSeparator: \.isNewline)
            .map({ $0.trimmingCharacters(in: .whitespaces) })
            .last(where: { $0.hasPrefix("{") })
        else { throw CLIOutputError.empty }
        return Data(line.utf8)
    }

    public static func syncReport(_ stdout: Data) throws -> SyncReport {
        try decoder.decode(SyncReport.self, from: lastJSONLine(stdout))
    }

    public static func sessionShow(_ stdout: Data) throws -> SessionShow {
        try decoder.decode(SessionShow.self, from: lastJSONLine(stdout))
    }

    /// Whether a CLI run failed because this build of the CLI does not know
    /// a flag or subcommand (clap's wording) — the app then falls back.
    public static func isUnsupported(_ stderr: String) -> Bool {
        let lower = stderr.lowercased()
        return lower.contains("unexpected argument") || lower.contains("unrecognized subcommand")
            || lower.contains("unknown argument")
    }

    public static func allowStatus(_ stdout: Data) throws -> AllowStatus {
        try decoder.decode(AllowStatus.self, from: lastJSONLine(stdout))
    }

    /// Read the opt-in file directly — the fallback when the CLI cannot run.
    /// A version this app does not know is refused, as the CLI refuses it.
    public static func allowlistFile(_ data: Data) throws -> AllowlistFile {
        let file = try decoder.decode(AllowlistFile.self, from: data)
        guard file.version == AllowlistFile.supportedVersion else {
            throw CLIOutputError.unsupportedAllowlistVersion(file.version)
        }
        return file
    }

    /// A login prompt, if this stderr line is one.
    public static func loginPrompt(_ line: String) -> LoginPrompt? {
        let trimmed = line.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("{") else { return nil }
        return try? decoder.decode(LoginPrompt.self, from: Data(trimmed.utf8))
    }

    /// The first line worth showing from a failed run's stderr: the message
    /// of `{"error":"…"}` in JSON mode, else the first non-empty line, with
    /// an `error: ` prefix dropped. Always scrubbed of token-shaped text.
    public static func firstErrorLine(_ stderr: String) -> String? {
        for raw in stderr.split(whereSeparator: \.isNewline) {
            let line = raw.trimmingCharacters(in: .whitespaces)
            if line.isEmpty { continue }
            if line.hasPrefix("{"),
               let object = try? JSONSerialization.jsonObject(with: Data(line.utf8)) as? [String: Any],
               let message = object["error"] as? String
            {
                let first = message.split(whereSeparator: \.isNewline).first.map(String.init) ?? message
                return Scrub.text(first)
            }
            if line.hasPrefix("{") { continue } // a JSON line that is not an error, e.g. a login prompt
            let stripped = line.hasPrefix("error: ") ? String(line.dropFirst(7)) : line
            return Scrub.text(stripped)
        }
        return nil
    }

    /// Whether an error message is the CLI's not-logged-in error
    /// (auth.rs, `token_for`), or an API 401 — which api.rs always suffixes
    /// with "run `livingbrain login`".
    public static func isNotLoggedIn(_ message: String) -> Bool {
        let lower = message.lowercased()
        return lower.contains("not logged in") || lower.contains("livingbrain login")
    }
}

/// Belt and braces: the CLI never prints a token, but nothing this app shows
/// should be able to carry one even if a future CLI or wrapper did.
public enum Scrub {
    static let patterns: [NSRegularExpression] = [
        // `Bearer abc…`, `token=abc…`, `token: abc…`
        // (Eight characters or more, so prose like "Bearer token required" survives.)
        try! NSRegularExpression(pattern: #"(?i)(bearer\s+)[^\s"']{8,}|(?i)(token\s*[=:]\s*)[^\s"']+"#),
        // Living Brain PATs (`lbp_…`) and any long opaque run.
        try! NSRegularExpression(pattern: #"\blbp_[A-Za-z0-9_\-]+"#),
        try! NSRegularExpression(pattern: #"(?<![/\w.-])[A-Za-z0-9_\-]{40,}(?![/\w.-])"#),
    ]

    public static func text(_ input: String) -> String {
        var out = input
        for (index, pattern) in patterns.enumerated() {
            let range = NSRange(out.startIndex..., in: out)
            let template = index == 0 ? "$1$2•••" : "•••"
            out = pattern.stringByReplacingMatches(in: out, range: range, withTemplate: template)
        }
        return out
    }
}
