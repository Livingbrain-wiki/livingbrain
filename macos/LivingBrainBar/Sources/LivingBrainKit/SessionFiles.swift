import Foundation

/// Finds the agents' session log files and reads the little the browser
/// needs straight from them — the last timestamp and the git branch — so the
/// list is complete even with a CLI that predates the `file`, `branch` and
/// `ended_at` report fields. Nothing here leaves the Mac.
public struct SessionFileIndex: Sendable {
    /// `agent/session_id` → absolute path.
    public private(set) var paths: [String: String] = [:]

    public init() {}

    /// Walk the default trees: `~/.claude/projects/<dir>/<id>.jsonl` and
    /// `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`.
    public static func scan(claudeRoot: String, codexRoot: String) -> SessionFileIndex {
        var index = SessionFileIndex()
        let fm = FileManager.default
        if let projects = try? fm.contentsOfDirectory(atPath: claudeRoot) {
            for project in projects {
                let dir = (claudeRoot as NSString).appendingPathComponent(project)
                for file in (try? fm.contentsOfDirectory(atPath: dir)) ?? [] where file.hasSuffix(".jsonl") {
                    let id = String(file.dropLast(".jsonl".count))
                    index.paths["claude_code/\(id)"] = (dir as NSString).appendingPathComponent(file)
                }
            }
        }
        if let walker = fm.enumerator(atPath: codexRoot) {
            for case let relative as String in walker where relative.hasSuffix(".jsonl") {
                guard let id = codexSessionId(fileName: (relative as NSString).lastPathComponent) else { continue }
                index.paths["codex/\(id)"] = (codexRoot as NSString).appendingPathComponent(relative)
            }
        }
        return index
    }

    /// The UUID at the end of a Codex rollout file name.
    public static func codexSessionId(fileName: String) -> String? {
        let stem = fileName.hasSuffix(".jsonl") ? String(fileName.dropLast(6)) : fileName
        guard stem.count >= 36 else { return nil }
        let id = String(stem.suffix(36))
        return UUID(uuidString: id) == nil ? nil : id
    }

    public mutating func setPath(_ path: String, forKey key: String) {
        paths[key] = path
    }

    public func path(agent: String, sessionId: String) -> String? {
        paths["\(agent)/\(sessionId)"]
    }
}

/// What the browser reads from one session file.
public struct SessionFileMeta: Equatable, Sendable {
    public var lastActivity: Date?
    public var branch: String?

    public init(lastActivity: Date? = nil, branch: String? = nil) {
        self.lastActivity = lastActivity
        self.branch = branch
    }

    /// The branch the session started on (from the head: Claude Code's
    /// first `gitBranch`, Codex's `session_meta` `git.branch` — what the
    /// CLI's record keeps) and the newest `timestamp` in the tail, falling
    /// back to the file's modification time.
    public static func read(path: String, tailBytes: Int = 64 * 1024, headBytes: Int = 64 * 1024) -> SessionFileMeta {
        guard let handle = FileHandle(forReadingAtPath: path) else { return SessionFileMeta() }
        defer { try? handle.close() }
        let head = String(decoding: handle.readData(ofLength: headBytes), as: UTF8.self)
        let size = (try? handle.seekToEnd()) ?? 0
        let start = size > UInt64(tailBytes) ? size - UInt64(tailBytes) : 0
        try? handle.seek(toOffset: start)
        let tail = String(decoding: handle.readDataToEndOfFile(), as: UTF8.self)
        var meta = SessionFileMeta(lastActivity: parse(tail: tail).lastActivity, branch: firstBranch(head))
        if meta.branch == nil { meta.branch = firstBranch(tail) }
        if meta.lastActivity == nil {
            meta.lastActivity = (try? FileManager.default.attributesOfItem(atPath: path)[.modificationDate]) as? Date
        }
        return meta
    }

    static let timestamp = try! NSRegularExpression(pattern: #""timestamp"\s*:\s*"([^"]+)""#)
    static let branch = try! NSRegularExpression(pattern: #""gitBranch"\s*:\s*"([^"]+)""#)
    static let codexBranch = try! NSRegularExpression(pattern: #""git"\s*:\s*\{[^{}]*?"branch"\s*:\s*"([^"]+)""#)

    /// The first branch a chunk of JSONL names.
    static func firstBranch(_ text: String) -> String? {
        let range = NSRange(text.startIndex..., in: text)
        for regex in [branch, codexBranch] {
            if let match = regex.firstMatch(in: text, range: range),
               let r = Range(match.range(at: 1), in: text), !text[r].isEmpty
            {
                return String(text[r])
            }
        }
        return nil
    }

    /// The last timestamp and last branch named in a chunk of JSONL.
    public static func parse(tail: String) -> SessionFileMeta {
        let range = NSRange(tail.startIndex..., in: tail)
        func last(_ regex: NSRegularExpression) -> String? {
            guard let match = regex.matches(in: tail, range: range).last,
                  let r = Range(match.range(at: 1), in: tail) else { return nil }
            return String(tail[r])
        }
        // The latest timestamp, not the last written: lines are chronological
        // but a trailing metadata line may carry an older one.
        let dates = timestamp.matches(in: tail, range: range).compactMap { match -> Date? in
            Range(match.range(at: 1), in: tail).flatMap { ISODate.parse(String(tail[$0])) }
        }
        return SessionFileMeta(lastActivity: dates.max(), branch: last(branch).flatMap { $0.isEmpty ? nil : $0 })
    }
}

/// How the browser names a repository: the basename of the git root that
/// contains the session's working directory, or the directory's own name
/// when it is not in a repository. `(home)` only for `$HOME` itself.
public enum RepoName {
    public static func display(repo: String, home: String, isRepoRoot: (String) -> Bool = RepoName.hasGitEntry) -> String {
        let path = RepoSummary.expand(repo, home: home)
        if path.isEmpty { return "(unknown)" }
        if path == home || path == home + "/" { return "(home)" }
        var current = path
        while current != "/" && !current.isEmpty && current != home {
            if isRepoRoot(current) { return (current as NSString).lastPathComponent }
            current = (current as NSString).deletingLastPathComponent
        }
        return (path as NSString).lastPathComponent
    }

    /// `.git` as a directory (a checkout) or a file (a worktree).
    public static func hasGitEntry(_ dir: String) -> Bool {
        FileManager.default.fileExists(atPath: (dir as NSString).appendingPathComponent(".git"))
    }
}
