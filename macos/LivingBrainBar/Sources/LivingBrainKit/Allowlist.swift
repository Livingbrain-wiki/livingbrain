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
    public var path: String
    public var id: String { path }

    public init(path: String) { self.path = path }

    /// The repo's folder name.
    public var name: String { (path as NSString).lastPathComponent }

    /// The path with the home prefix shortened to `~`.
    public func displayPath(home: String = NSHomeDirectory()) -> String {
        if path == home { return "~" }
        if path.hasPrefix(home + "/") { return "~" + path.dropFirst(home.count) }
        return path
    }
}
