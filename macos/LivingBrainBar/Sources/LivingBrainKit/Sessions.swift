import Foundation

/// ISO 8601 with or without fractional seconds, as the agents write them.
public enum ISODate {
    public static func parse(_ text: String) -> Date? {
        let withFraction = ISO8601DateFormatter()
        withFraction.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let date = withFraction.date(from: text) { return date }
        let plain = ISO8601DateFormatter()
        plain.formatOptions = [.withInternetDateTime]
        return plain.date(from: text)
    }
}

/// `livingbrain logs show <file> --json`: the redacted record a sync sends.
public struct SessionShow: Decodable, Equatable, Sendable {
    public var file: String
    public var optedIn: Bool
    public var redacted: Int
    public var record: SessionRecord

    enum CodingKeys: String, CodingKey {
        case file, redacted, record
        case optedIn = "opted_in"
    }
}

/// The normalised session schema, version 1 (logs.rs `SessionRecord`).
public struct SessionRecord: Decodable, Equatable, Sendable {
    public var schemaVersion: Int
    public var agent: String
    public var sessionId: String
    public var repo: String
    public var branch: String?
    public var models: [String]
    public var startedAt: String?
    public var endedAt: String?
    public var turns: [Turn]
    public var truncated: Bool

    public struct Turn: Decodable, Equatable, Sendable, Identifiable {
        public var role: String
        public var at: String?
        public var text: String
        public var toolCalls: [String]
        /// Position in the record; set after decoding.
        public var id: Int = 0

        enum CodingKeys: String, CodingKey {
            case role, at, text
            case toolCalls = "tool_calls"
        }

        struct ToolCall: Decodable { var name: String }

        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            role = try c.decodeIfPresent(String.self, forKey: .role) ?? ""
            at = try c.decodeIfPresent(String.self, forKey: .at)
            text = try c.decodeIfPresent(String.self, forKey: .text) ?? ""
            toolCalls = (try c.decodeIfPresent([ToolCall].self, forKey: .toolCalls) ?? []).map(\.name)
        }
    }

    enum CodingKeys: String, CodingKey {
        case agent, repo, branch, models, turns, truncated
        case schemaVersion = "schema_version"
        case sessionId = "session_id"
        case startedAt = "started_at"
        case endedAt = "ended_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        schemaVersion = try c.decodeIfPresent(Int.self, forKey: .schemaVersion) ?? 1
        agent = try c.decodeIfPresent(String.self, forKey: .agent) ?? ""
        sessionId = try c.decodeIfPresent(String.self, forKey: .sessionId) ?? ""
        repo = try c.decodeIfPresent(String.self, forKey: .repo) ?? ""
        branch = try c.decodeIfPresent(String.self, forKey: .branch)
        models = try c.decodeIfPresent([String].self, forKey: .models) ?? []
        startedAt = try c.decodeIfPresent(String.self, forKey: .startedAt)
        endedAt = try c.decodeIfPresent(String.self, forKey: .endedAt)
        truncated = try c.decodeIfPresent(Bool.self, forKey: .truncated) ?? false
        var decoded = try c.decodeIfPresent([Turn].self, forKey: .turns) ?? []
        for index in decoded.indices { decoded[index].id = index }
        turns = decoded
    }
}

/// What this Mac knows about sessions it has already synced, keyed by
/// `agent/session_id`. Kept in Application Support as JSON; it holds ids and
/// timestamps only, never session content.
public struct SessionLedger: Codable, Equatable, Sendable {
    public struct Entry: Codable, Equatable, Sendable {
        public var syncedAt: Date
        public var sourceId: String?
        public var noteSlug: String?
        public var noteURL: String?
    }

    public private(set) var entries: [String: Entry] = [:]

    public init() {}

    /// Record a real (not dry-run) sync's outcome.
    public mutating func record(_ report: SyncReport, at date: Date) {
        guard !report.dryRun else { return }
        for session in report.sessions where session.status == "uploaded" || session.status == "unchanged" {
            var entry = entries[session.key] ?? Entry(syncedAt: date)
            entry.syncedAt = date
            entry.sourceId = session.id ?? entry.sourceId
            entry.noteSlug = session.note?.slug ?? entry.noteSlug
            entry.noteURL = session.note?.url ?? entry.noteURL
            entries[session.key] = entry
        }
    }

    public func entry(for session: SyncedSession) -> Entry? { entries[session.key] }

    public static func load(from url: URL) -> SessionLedger {
        guard let data = try? Data(contentsOf: url) else { return SessionLedger() }
        let decoder = JSONDecoder()
        decoder.dateDecodingStrategy = .iso8601
        return (try? decoder.decode(SessionLedger.self, from: data)) ?? SessionLedger()
    }

    public func save(to url: URL) throws {
        let encoder = JSONEncoder()
        encoder.dateEncodingStrategy = .iso8601
        encoder.outputFormatting = [.sortedKeys]
        try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try encoder.encode(self).write(to: url, options: .atomic)
    }
}

/// One row of the Recent Sessions browser: a local session plus what the
/// ledger knows about it.
public struct SessionRow: Identifiable, Equatable, Sendable {
    public enum SyncStatus: Equatable, Sendable {
        case notOptedIn
        case pending
        case synced(Date)
    }

    public var session: SyncedSession
    public var ledger: SessionLedger.Entry?
    public var id: String { session.key }

    public var status: SyncStatus {
        if !session.isOptedIn { return .notOptedIn }
        if let entry = ledger { return .synced(entry.syncedAt) }
        return .pending
    }

    public var statusLabel: String {
        switch status {
        case .notOptedIn: return "Not opted in"
        case .pending: return "Not synced yet"
        case .synced: return "Synced"
        }
    }

    /// The page to open in Living Brain: the session's note when known, else
    /// the project's session list.
    public func webURL(base: URL) -> URL {
        let slug = ledger?.noteSlug ?? (session.isOptedIn ? session.note?.slug : nil)
        var components = URLComponents(url: base.appendingPathComponent("wiki.html"), resolvingAgainstBaseURL: false)!
        if let slug, ledger != nil {
            components.queryItems = [URLQueryItem(name: "slug", value: slug)]
        } else {
            components.queryItems = [
                URLQueryItem(name: "view", value: "sessions"),
                URLQueryItem(name: "project", value: session.projectName),
            ]
        }
        return components.url!
    }

    /// Build rows from a dry-run report, newest first.
    public static func rows(from report: SyncReport, ledger: SessionLedger) -> [SessionRow] {
        report.sessions
            .map { SessionRow(session: $0, ledger: ledger.entry(for: $0)) }
            .sorted { ($0.session.endedDate ?? .distantPast) > ($1.session.endedDate ?? .distantPast) }
    }
}

/// A repository seen in local sessions, with its opt-in state.
public struct RepoSummary: Identifiable, Equatable, Sendable {
    /// Absolute path, `~` expanded.
    public var path: String
    public var sessions: Int
    public var optedIn: Bool
    public var lastActivity: Date?
    public var id: String { path }
    public var name: String { (path as NSString).lastPathComponent }

    /// Every repo the sessions mention, plus every allowed path, sorted by
    /// most recent activity. A repo is opted in when an allowed path is it or
    /// one of its ancestors (the CLI's rule).
    public static func summarise(sessions: [SyncedSession], allowed: [String], home: String) -> [RepoSummary] {
        var byPath: [String: RepoSummary] = [:]
        for session in sessions where !session.repo.isEmpty {
            let path = expand(session.repo, home: home)
            var summary = byPath[path] ?? RepoSummary(path: path, sessions: 0, optedIn: false, lastActivity: nil)
            summary.sessions += 1
            if let ended = session.endedDate, ended > (summary.lastActivity ?? .distantPast) {
                summary.lastActivity = ended
            }
            byPath[path] = summary
        }
        for path in allowed where byPath[path] == nil {
            byPath[path] = RepoSummary(path: path, sessions: 0, optedIn: true, lastActivity: nil)
        }
        for key in byPath.keys {
            byPath[key]!.optedIn = allowed.contains { isInside(key, $0) }
        }
        return byPath.values.sorted {
            ($0.lastActivity ?? .distantPast, $1.name) > ($1.lastActivity ?? .distantPast, $0.name)
        }
    }

    public static func expand(_ repo: String, home: String) -> String {
        if repo == "~" { return home }
        if repo.hasPrefix("~/") { return home + repo.dropFirst(1) }
        return repo
    }

    /// Component-wise containment, as logs.rs `is_inside`.
    public static func isInside(_ child: String, _ ancestor: String) -> Bool {
        child == ancestor || child.hasPrefix(ancestor.hasSuffix("/") ? ancestor : ancestor + "/")
    }
}
