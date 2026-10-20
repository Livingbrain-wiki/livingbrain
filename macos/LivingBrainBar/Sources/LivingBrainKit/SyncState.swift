import Foundation

/// What the menu bar icon shows.
public enum SyncPhase: Equatable, Sendable {
    case idle
    case syncing
    /// The first line of the CLI's stderr (already scrubbed).
    case error(String)
}

/// The menu's state machine, kept free of AppKit so it can be tested.
public struct SyncState: Equatable, Sendable {
    public private(set) var phase: SyncPhase = .idle
    public private(set) var lastSyncAt: Date?
    public private(set) var lastReport: SyncReport?
    /// Set when the CLI says it has no token; cleared by a successful sync or
    /// sign-in.
    public private(set) var needsSignIn = false

    public init() {}

    public var isSyncing: Bool { phase == .syncing }

    public var errorMessage: String? {
        if case let .error(message) = phase { return message }
        return nil
    }

    /// What to do about the error, when the error has a known fix.
    public var errorHint: String? {
        errorMessage.flatMap(CLIOutput.optInHint)
    }

    /// Start a sync. Returns false (and changes nothing) when one is already
    /// running, so a timer tick and a click never run two CLIs at once.
    @discardableResult
    public mutating func begin() -> Bool {
        guard phase != .syncing else { return false }
        phase = .syncing
        return true
    }

    public mutating func succeed(_ report: SyncReport, at date: Date) {
        phase = .idle
        lastReport = report
        lastSyncAt = date
        needsSignIn = false
    }

    /// A failed run: `stderr` is the CLI's raw stderr, `status` its exit code.
    public mutating func fail(stderr: String, status: Int32) {
        let message = CLIOutput.firstErrorLine(stderr) ?? "livingbrain exited with status \(status)"
        if CLIOutput.isNotLoggedIn(message) { needsSignIn = true }
        phase = .error(message)
    }

    /// A failure that never reached the CLI (missing binary, bad JSON).
    public mutating func fail(message: String) {
        phase = .error(Scrub.text(message))
    }

    public mutating func signedIn() {
        needsSignIn = false
        if case .error = phase { phase = .idle }
    }

    public mutating func requireSignIn() {
        needsSignIn = true
    }

    /// The "Last sync" line.
    public func lastSyncLine(now: Date = Date(), locale: Locale = .current) -> String {
        guard let at = lastSyncAt else { return "Not synced yet" }
        let formatter = RelativeDateTimeFormatter()
        formatter.locale = locale
        formatter.unitsStyle = .full
        let seconds = now.timeIntervalSince(at)
        let when = seconds < 60 ? "just now" : formatter.localizedString(for: at, relativeTo: now)
        return "Last sync \(when)"
    }
}

/// Auto-sync cadence.
public enum AutoSync {
    public static let interval: TimeInterval = 15 * 60

    /// Whether a periodic tick should start a sync now.
    public static func isDue(lastSyncAt: Date?, now: Date, interval: TimeInterval = AutoSync.interval) -> Bool {
        guard let last = lastSyncAt else { return true }
        // A little slack so a timer that fires a hair early still syncs.
        return now.timeIntervalSince(last) >= interval - 5
    }
}
