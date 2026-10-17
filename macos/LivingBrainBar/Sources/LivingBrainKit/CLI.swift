import Foundation

/// Where the `livingbrain` binary is, and how it is run.
public enum CLILocator {
    /// Checked in order; the first that exists wins.
    public static func candidates(home: String) -> [String] {
        [
            (home as NSString).appendingPathComponent("bin/livingbrain"),
            "/opt/homebrew/bin/livingbrain",
            "/usr/local/bin/livingbrain",
        ]
    }

    public static func defaultPath(
        home: String = NSHomeDirectory(),
        isExecutable: (String) -> Bool = { FileManager.default.isExecutableFile(atPath: $0) }
    ) -> String? {
        candidates(home: home).first(where: isExecutable)
    }

    /// The copy shipped inside the app bundle (`Contents/Resources/livingbrain`,
    /// put there by `scripts/release.sh`), or nil when running from a plain
    /// `swift build`.
    public static func bundledPath(bundle: Bundle = .main) -> String? {
        bundle.resourceURL?.appendingPathComponent("livingbrain").path
    }

    /// The configured path when set and executable, else the CLI bundled with
    /// the app, else the first executable default location.
    public static func resolve(
        configured: String?,
        home: String = NSHomeDirectory(),
        bundled: String? = bundledPath(),
        isExecutable: (String) -> Bool = { FileManager.default.isExecutableFile(atPath: $0) }
    ) -> String? {
        if let configured, !configured.isEmpty {
            let expanded = (configured as NSString).expandingTildeInPath
            if isExecutable(expanded) { return expanded }
        }
        if let bundled, isExecutable(bundled) { return bundled }
        return defaultPath(home: home, isExecutable: isExecutable)
    }
}

/// One workspace's API token, as the app holds it in its own keychain items.
public struct CLIWorkspaceToken: Sendable, Equatable {
    public let id: String
    public let token: String

    public init(id: String, token: String) {
        self.id = id
        self.token = token
    }
}

/// The tokens the app hands its CLI child, so the CLI never reads a keychain
/// item that is not its own (every read from another app's item prompts).
///
/// They travel only in the child's environment (see [`CLIEnvironment.make`]),
/// never in arguments, and this type never prints them: its descriptions show
/// a count. Environment variables are readable only by the same user's
/// processes, and the CLI scrubs them from every child it starts.
public struct CLITokenHandoff: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    /// The default workspace's token (`LIVINGBRAIN_TOKEN`): the token the
    /// default destination and `jobs run` use.
    public let defaultToken: String?
    /// The workspace that token must belong to (`LIVINGBRAIN_DEFAULT_WORKSPACE`);
    /// the CLI confirms it once per run before using the token.
    public let defaultWorkspaceId: String?
    public let workspaces: [CLIWorkspaceToken]
    /// The server the tokens belong to. Set over any inherited
    /// `LIVINGBRAIN_API_URL`, so a token never meets another server.
    public let apiURL: String?

    public init(
        defaultToken: String? = nil,
        defaultWorkspaceId: String? = nil,
        workspaces: [CLIWorkspaceToken] = [],
        apiURL: String? = nil
    ) {
        self.defaultToken = defaultToken
        self.defaultWorkspaceId = defaultWorkspaceId
        self.workspaces = workspaces
        self.apiURL = apiURL
    }

    /// The variables this handoff sets. Two workspace ids that normalise to the
    /// same variable name would share it, so neither is handed over: the CLI
    /// then holds sessions for both rather than send one into the other.
    public var variables: [String: String] {
        var env: [String: String] = [:]
        if let defaultToken, !defaultToken.isEmpty {
            env["LIVINGBRAIN_TOKEN"] = defaultToken
            if let defaultWorkspaceId, !defaultWorkspaceId.isEmpty {
                env["LIVINGBRAIN_DEFAULT_WORKSPACE"] = defaultWorkspaceId
            }
        }
        let groups = Dictionary(grouping: workspaces, by: { CLIEnvironment.workspaceVariable($0.id) })
        for (name, group) in groups where group.count == 1 && !group[0].token.isEmpty {
            env[name] = group[0].token
        }
        return env
    }

    public var description: String {
        "CLITokenHandoff(\(workspaces.count) workspace\(workspaces.count == 1 ? "" : "s"), tokens redacted)"
    }

    public var debugDescription: String { description }
}

public enum CLIEnvironment {
    public static let defaultAPIURL = "https://app.livingbrain.wiki"

    /// Tells the CLI to leave the keychain alone for this run.
    public static let noKeychainVariable = "LIVINGBRAIN_NO_KEYCHAIN"
    /// Tells the CLI the machine signs in to workspaces, so a bare
    /// `logs allow` is refused (see `logs allow` in the CLI).
    public static let multiWorkspaceVariable = "LIVINGBRAIN_MULTI_WORKSPACE"

    /// The environment variable for one workspace's token, spelled as the CLI's
    /// `workspace_env_var`: the id upper-cased, every other character `_`.
    public static func workspaceVariable(_ id: String) -> String {
        let suffix = id.map { character -> Character in
            character.isASCII && (character.isLetter || character.isNumber)
                ? Character(character.uppercased())
                : "_"
        }
        return "LIVINGBRAIN_TOKEN_" + String(suffix)
    }

    /// The child's environment: ours, plus `LIVINGBRAIN_API_URL` when unset.
    /// An app started from Finder has a bare PATH, so the usual tool
    /// directories are appended for wrapper scripts.
    ///
    /// With a `handoff`, every token variable inherited from our own
    /// environment is dropped (the child sees exactly what the app decided),
    /// the handed tokens are set, and the keychain is switched off for the run.
    /// Without one, the environment is passed on as before (`login` needs the
    /// keychain, to save the CLI's sign-in).
    public static func make(
        base: [String: String],
        apiURL: String = defaultAPIURL,
        handoff: CLITokenHandoff? = nil
    ) -> [String: String] {
        var env = base
        if let handoff {
            for key in env.keys.filter({ $0 == "LIVINGBRAIN_TOKEN" || $0.hasPrefix("LIVINGBRAIN_TOKEN_") }) {
                env.removeValue(forKey: key)
            }
            env.merge(handoff.variables) { _, handed in handed }
            env[noKeychainVariable] = "1"
            if handoff.workspaces.isEmpty && handoff.defaultWorkspaceId == nil {
                env.removeValue(forKey: multiWorkspaceVariable)
            } else {
                env[multiWorkspaceVariable] = "1"
            }
            env["LIVINGBRAIN_API_URL"] = handoff.apiURL ?? apiURL
        } else if (env["LIVINGBRAIN_API_URL"] ?? "").isEmpty {
            env["LIVINGBRAIN_API_URL"] = apiURL
        }
        let extra = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        var path = (env["PATH"] ?? "").split(separator: ":").map(String.init)
        for dir in extra where !path.contains(dir) { path.append(dir) }
        env["PATH"] = path.joined(separator: ":")
        return env
    }
}

/// The outcome of one CLI run.
public struct CLIResult: Sendable {
    public var status: Int32
    public var stdout: Data
    public var stderr: String

    public init(status: Int32, stdout: Data, stderr: String) {
        self.status = status
        self.stdout = stdout
        self.stderr = stderr
    }

    public var succeeded: Bool { status == 0 }
}

public enum CLIRunError: Error, LocalizedError {
    case notFound
    case launch(String)

    public var errorDescription: String? {
        switch self {
        case .notFound:
            return "livingbrain CLI not found — set its path under Settings"
        case let .launch(message):
            return "could not start livingbrain: \(message)"
        }
    }
}

/// A running CLI process, so a long `login` can be cancelled.
public final class CLIProcess: @unchecked Sendable {
    let process = Process()
    public init() {}
    public func terminate() {
        if process.isRunning { process.terminate() }
    }
}

public enum CLIRunner {
    /// Run `executable args…`, collecting stdout and stderr. `onStderrLine`
    /// sees each stderr line as it arrives (for `login`'s prompt).
    public static func run(
        executable: String,
        arguments: [String],
        environment: [String: String],
        handle: CLIProcess = CLIProcess(),
        onStderrLine: (@Sendable (String) -> Void)? = nil
    ) async throws -> CLIResult {
        let process = handle.process
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.environment = environment
        process.currentDirectoryURL = URL(fileURLWithPath: NSHomeDirectory())
        process.standardInput = FileHandle.nullDevice
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err

        let collector = Collector(onStderrLine: onStderrLine)
        out.fileHandleForReading.readabilityHandler = { handle in
            collector.appendOut(handle.availableData)
        }
        err.fileHandleForReading.readabilityHandler = { handle in
            collector.appendErr(handle.availableData)
        }

        return try await withCheckedThrowingContinuation { continuation in
            process.terminationHandler = { process in
                out.fileHandleForReading.readabilityHandler = nil
                err.fileHandleForReading.readabilityHandler = nil
                collector.appendOut(out.fileHandleForReading.readDataToEndOfFile())
                collector.appendErr(err.fileHandleForReading.readDataToEndOfFile())
                collector.flush()
                continuation.resume(returning: collector.result(status: process.terminationStatus))
            }
            do {
                try process.run()
            } catch {
                process.terminationHandler = nil
                out.fileHandleForReading.readabilityHandler = nil
                err.fileHandleForReading.readabilityHandler = nil
                continuation.resume(throwing: CLIRunError.launch(error.localizedDescription))
            }
        }
    }
}

/// Thread-safe accumulation of a child's output.
final class Collector: @unchecked Sendable {
    private let lock = NSLock()
    private var out = Data()
    private var err = Data()
    private var pending = Data()
    private let onStderrLine: (@Sendable (String) -> Void)?

    init(onStderrLine: (@Sendable (String) -> Void)?) {
        self.onStderrLine = onStderrLine
    }

    func appendOut(_ data: Data) {
        guard !data.isEmpty else { return }
        lock.lock(); out.append(data); lock.unlock()
    }

    func appendErr(_ data: Data) {
        guard !data.isEmpty else { return }
        lock.lock()
        err.append(data)
        pending.append(data)
        var lines: [String] = []
        while let newline = pending.firstIndex(of: 0x0A) {
            lines.append(String(decoding: pending[pending.startIndex..<newline], as: UTF8.self))
            pending.removeSubrange(pending.startIndex...newline)
        }
        lock.unlock()
        lines.forEach { onStderrLine?($0) }
    }

    func flush() {
        lock.lock()
        let rest = pending
        pending = Data()
        lock.unlock()
        if !rest.isEmpty { onStderrLine?(String(decoding: rest, as: UTF8.self)) }
    }

    func result(status: Int32) -> CLIResult {
        lock.lock(); defer { lock.unlock() }
        return CLIResult(status: status, stdout: out, stderr: String(decoding: err, as: UTF8.self))
    }
}
