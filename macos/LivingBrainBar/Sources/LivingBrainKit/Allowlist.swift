import Foundation

/// The opt-in file's location, as logs.rs `config_dir` computes it:
/// `$XDG_CONFIG_HOME/livingbrain`, else `~/.config/livingbrain`.
public enum AllowlistLocation {
    public static func path(environment: [String: String], home: String) -> String {
        if let xdg = environment["XDG_CONFIG_HOME"], !xdg.isEmpty {
            return (xdg as NSString).appendingPathComponent("livingbrain/logs-allow.json")
        }
        return (home as NSString).appendingPathComponent(".config/livingbrain/logs-allow.json")
    }

    /// The allowed repositories read straight from disk; a missing file is
    /// the ordinary nothing-opted-in case.
    public static func read(path: String) throws -> [String] {
        guard FileManager.default.fileExists(atPath: path) else { return [] }
        let data = try Data(contentsOf: URL(fileURLWithPath: path))
        return try CLIOutput.allowlistFile(data).allowed
    }
}

/// A project as the menu shows it.
public struct SyncedProject: Identifiable, Equatable, Sendable {
    /// The workspace the default (`login`'s) token names.
    public static let defaultWorkspace = "default"
    /// The picker's word for "no explicit choice: the routes decide".
    public static let automaticWorkspace = "auto"

    public var path: String
    /// The explicit choice: a workspace id, `default`, or nil for automatic.
    public var explicit: String?
    /// Where the sessions go now: a workspace id, `default`, or `unroutable`.
    public var effective: String
    /// The route that chose `effective`, or why no route could decide.
    public var routeReason: String?
    /// The parent folder whose explicit choice this project follows, when it
    /// names none of its own.
    public var inheritedFrom: String?
    /// The repository's own top level, when git could read one: what a
    /// "Pin This Repository" action pins.
    public var repoTop: String?
    /// For a folder that holds checkouts: each one's own answer.
    public var nested: [NestedRepo]
    public var id: String { path }

    public init(
        path: String, explicit: String? = nil, effective: String = SyncedProject.defaultWorkspace,
        routeReason: String? = nil, inheritedFrom: String? = nil, repoTop: String? = nil,
        nested: [NestedRepo] = []
    ) {
        self.path = path
        self.explicit = explicit
        self.effective = effective
        self.routeReason = routeReason
        self.inheritedFrom = inheritedFrom
        self.repoTop = repoTop
        self.nested = nested
    }

    /// The repo's folder name.
    public var name: String { (path as NSString).lastPathComponent }

    /// The path with the home prefix shortened to `~`.
    public func displayPath(home: String = NSHomeDirectory()) -> String {
        if path == home { return "~" }
        if path.hasPrefix(home + "/") { return "~" + path.dropFirst(home.count) }
        return path
    }
}

/// A checkout inside an allowed folder, with the workspace it sends to.
public struct NestedRepo: Decodable, Identifiable, Equatable, Sendable {
    public var path: String
    public var effective: String
    public var routeReason: String?

    public var id: String { path }

    enum CodingKeys: String, CodingKey {
        case path, effective
        case routeReason = "route_reason"
    }

    public init(path: String, effective: String, routeReason: String? = nil) {
        self.path = path
        self.effective = effective
        self.routeReason = routeReason
    }
}
