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

    /// The configured path when set and executable, else the default.
    public static func resolve(
        configured: String?,
        home: String = NSHomeDirectory(),
        isExecutable: (String) -> Bool = { FileManager.default.isExecutableFile(atPath: $0) }
    ) -> String? {
        if let configured, !configured.isEmpty {
            let expanded = (configured as NSString).expandingTildeInPath
            if isExecutable(expanded) { return expanded }
        }
        return defaultPath(home: home, isExecutable: isExecutable)
    }
}

public enum CLIEnvironment {
    public static let defaultAPIURL = "https://app.livingbrain.wiki"

    /// The child's environment: ours, plus `LIVINGBRAIN_API_URL` when unset.
    /// An app started from Finder has a bare PATH, so the usual tool
    /// directories are appended for wrapper scripts.
    public static func make(base: [String: String], apiURL: String = defaultAPIURL) -> [String: String] {
        var env = base
        if (env["LIVINGBRAIN_API_URL"] ?? "").isEmpty {
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
