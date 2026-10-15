import CoreServices
import Foundation

/// Tracks session files that changed and reports each one once it has been
/// quiet for `idle` seconds — an agent appends to its log on every turn, so
/// a file that stopped changing is a session that paused or ended.
public struct IdleDebouncer: Equatable, Sendable {
    public static let defaultIdle: TimeInterval = 120

    public let idle: TimeInterval
    /// Last change seen per file.
    public private(set) var pending: [String: Date] = [:]

    public init(idle: TimeInterval = IdleDebouncer.defaultIdle) {
        self.idle = idle
    }

    /// Whether a changed path is a session log this app cares about.
    public static func isSessionLog(_ path: String) -> Bool {
        path.lowercased().hasSuffix(".jsonl")
    }

    /// Note a change. Non-session paths are ignored.
    public mutating func touch(_ path: String, at date: Date) {
        guard Self.isSessionLog(path) else { return }
        pending[path] = max(pending[path] ?? date, date)
    }

    /// The files quiet for at least `idle` seconds, oldest first; they leave
    /// the pending set. A file touched again later comes back.
    public mutating func takeDue(now: Date) -> [String] {
        let due = pending
            .filter { now.timeIntervalSince($0.value) >= idle }
            .sorted { $0.value == $1.value ? $0.key < $1.key : $0.value < $1.value }
            .map(\.key)
        for path in due { pending[path] = nil }
        return due
    }

    /// When the next file becomes due, if any.
    public func nextDue() -> Date? {
        pending.values.min().map { $0.addingTimeInterval(idle) }
    }
}

/// Rate-limits user notifications per kind: at most one per `interval`.
/// Uploads that land inside the window are counted and folded into the next
/// notification instead of being dropped.
public struct NotificationThrottle: Equatable, Sendable {
    public enum Kind: String, Sendable { case upload, error }

    public var intervals: [Kind: TimeInterval]
    public private(set) var lastSent: [Kind: Date] = [:]
    public private(set) var lastErrorMessage: String?
    /// Uploads held back by the window, reported with the next one.
    public private(set) var heldUploads = 0

    public init(upload: TimeInterval = 10 * 60, error: TimeInterval = 30 * 60) {
        intervals = [.upload: upload, .error: error]
    }

    /// Whether to show an upload notification now, and for how many
    /// sessions (including any held back earlier).
    public mutating func upload(count: Int, now: Date) -> Int? {
        guard count > 0 else { return nil }
        if let last = lastSent[.upload], now.timeIntervalSince(last) < intervals[.upload]! {
            heldUploads += count
            return nil
        }
        lastSent[.upload] = now
        let total = count + heldUploads
        heldUploads = 0
        return total
    }

    /// Whether to show an error notification now. A different message
    /// always gets through; the same one waits out the window.
    public mutating func error(_ message: String, now: Date) -> Bool {
        if message == lastErrorMessage, let last = lastSent[.error],
           now.timeIntervalSince(last) < intervals[.error]!
        {
            return false
        }
        lastSent[.error] = now
        lastErrorMessage = message
        return true
    }
}

/// FSEvents over the agents' log trees. Calls `onChange` with each changed
/// file path, on the main queue.
public final class SessionWatcher: @unchecked Sendable {
    public static func defaultRoots(home: String = NSHomeDirectory()) -> [String] {
        [
            (home as NSString).appendingPathComponent(".claude/projects"),
            (home as NSString).appendingPathComponent(".codex/sessions"),
        ]
    }

    private var stream: FSEventStreamRef?
    private let onChange: (String) -> Void
    public private(set) var watching: [String] = []

    public init(onChange: @escaping (String) -> Void) {
        self.onChange = onChange
    }

    deinit { stop() }

    /// Start watching whichever roots exist. Returns the roots watched.
    @discardableResult
    public func start(roots: [String] = SessionWatcher.defaultRoots(), latency: TimeInterval = 3) -> [String] {
        stop()
        let existing = roots.filter { FileManager.default.fileExists(atPath: $0) }
        guard !existing.isEmpty else { return [] }
        var context = FSEventStreamContext(
            version: 0,
            info: Unmanaged.passUnretained(self).toOpaque(),
            retain: nil, release: nil, copyDescription: nil
        )
        let callback: FSEventStreamCallback = { _, info, count, paths, flags, _ in
            guard let info else { return }
            let watcher = Unmanaged<SessionWatcher>.fromOpaque(info).takeUnretainedValue()
            let array = Unmanaged<CFArray>.fromOpaque(paths).takeUnretainedValue() as NSArray
            for index in 0..<count {
                let flag = flags[index]
                let isFile = flag & UInt32(kFSEventStreamEventFlagItemIsFile) != 0
                guard isFile, let path = array[index] as? String else { continue }
                watcher.onChange(path)
            }
        }
        let flags = UInt32(
            kFSEventStreamCreateFlagFileEvents | kFSEventStreamCreateFlagUseCFTypes
                | kFSEventStreamCreateFlagNoDefer
        )
        guard let stream = FSEventStreamCreate(
            nil, callback, &context, existing as CFArray,
            FSEventStreamEventId(kFSEventStreamEventIdSinceNow), latency, flags
        ) else { return [] }
        FSEventStreamSetDispatchQueue(stream, .main)
        FSEventStreamStart(stream)
        self.stream = stream
        watching = existing
        return existing
    }

    public func stop() {
        guard let stream else { return }
        FSEventStreamStop(stream)
        FSEventStreamInvalidate(stream)
        FSEventStreamRelease(stream)
        self.stream = nil
        watching = []
    }
}
