import AppKit
import Foundation
import LivingBrainKit
import ServiceManagement
import UserNotifications

/// The sync side of the app: runs the CLI, keeps `SyncState`, watches the
/// agents' log trees, and owns the settings that live in `UserDefaults`.
@MainActor
final class BarModel: ObservableObject {
    enum Keys {
        static let autoSync = "autoSync"
        static let liveSync = "liveSync"
        static let paused = "paused"
        static let notifications = "notifications"
        static let cliPath = "cliPath"
        static let serverURL = "serverURL"
    }

    @Published private(set) var state = SyncState()
    @Published private(set) var projects: [SyncedProject] = []
    @Published private(set) var projectsError: String?
    /// Sessions the last sync held because their workspace is not signed in.
    @Published private(set) var held: [HeldSessions] = []
    @Published private(set) var loginPrompt: LoginPrompt?
    @Published private(set) var signingIn = false
    @Published private(set) var launchAtLogin = false
    @Published private(set) var launchAtLoginError: String?
    @Published private(set) var ledger: SessionLedger
    /// Session files waiting to go quiet, for the menu's "watching" line.
    @Published private(set) var pendingFiles = 0
    @Published private(set) var watchedRoots: [String] = []
    /// Log files whose last live sync failed, with the CLI's message.
    @Published private(set) var failedFiles: [String: String] = [:]

    @Published var autoSync: Bool {
        didSet { defaults.set(autoSync, forKey: Keys.autoSync); scheduleTimer() }
    }
    @Published var liveSync: Bool {
        didSet { defaults.set(liveSync, forKey: Keys.liveSync); configureWatcher() }
    }
    @Published var paused: Bool {
        didSet { defaults.set(paused, forKey: Keys.paused) }
    }
    @Published var notifications: Bool {
        didSet {
            defaults.set(notifications, forKey: Keys.notifications)
            if notifications { requestNotificationPermission() }
        }
    }
    @Published var serverURLText: String {
        didSet { defaults.set(serverURLText, forKey: Keys.serverURL) }
    }

    private var timer: Timer?
    private var liveTimer: Timer?
    private var loginProcess: CLIProcess?
    private let defaults = UserDefaults.standard
    private var debouncer = IdleDebouncer()
    private var throttle = NotificationThrottle()
    private var watcher: SessionWatcher?
    /// Cleared the first time the CLI rejects `--session` (an older build).
    private var cliSupportsSessionFlag = true
    /// Files that went quiet while a sync was running; picked up next.
    private var queuedFiles: Set<String> = []

    static let ledgerURL: URL = {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent("Library/Application Support")
        return base.appendingPathComponent("Living Brain/sessions-ledger.json")
    }()

    init() {
        let defaults = UserDefaults.standard
        defaults.register(defaults: [Keys.liveSync: true, Keys.notifications: true])
        autoSync = defaults.bool(forKey: Keys.autoSync)
        liveSync = defaults.bool(forKey: Keys.liveSync)
        paused = defaults.bool(forKey: Keys.paused)
        notifications = defaults.bool(forKey: Keys.notifications)
        serverURLText = defaults.string(forKey: Keys.serverURL) ?? CLIEnvironment.defaultAPIURL
        ledger = SessionLedger.load(from: Self.ledgerURL)
        launchAtLogin = SMAppService.mainApp.status == .enabled
        scheduleTimer()
        configureWatcher()
        Task { await refreshProjects() }
    }

    /// The web app and API origin.
    var serverURL: URL {
        URL(string: serverURLText.trimmingCharacters(in: .whitespaces)).flatMap { $0.scheme == nil ? nil : $0 }
            ?? URL(string: CLIEnvironment.defaultAPIURL)!
    }

    var appURL: URL { serverURL.appendingPathComponent("") }
    var sessionsURL: URL {
        var c = URLComponents(url: serverURL.appendingPathComponent("wiki.html"), resolvingAgainstBaseURL: false)!
        c.queryItems = [URLQueryItem(name: "view", value: "sessions")]
        return c.url!
    }

    // MARK: CLI

    var cliPath: String? {
        CLILocator.resolve(configured: defaults.string(forKey: Keys.cliPath))
    }

    var cliPathLabel: String {
        guard let path = cliPath else { return "CLI: not found" }
        return "CLI: " + SyncedProject(path: path).displayPath()
    }

    func runCLI(_ arguments: [String], onStderrLine: (@Sendable (String) -> Void)? = nil,
                handle: CLIProcess = CLIProcess()) async throws -> CLIResult
    {
        guard let path = cliPath else { throw CLIRunError.notFound }
        return try await CLIRunner.run(
            executable: path,
            arguments: arguments,
            environment: CLIEnvironment.make(base: ProcessInfo.processInfo.environment, apiURL: serverURL.absoluteString),
            handle: handle,
            onStderrLine: onStderrLine
        )
    }

    // MARK: Sync

    /// Sync everything opted in, or only `files` (live sync). The CLI decides
    /// what may upload: a session outside an opted-in repo never leaves the
    /// machine. This app only asks.
    func syncNow(files: [String] = [], userInitiated: Bool = true) {
        guard state.begin() else {
            queuedFiles.formUnion(files)
            return
        }
        Task {
            var arguments = ["--json", "logs", "sync"]
            let narrow = !files.isEmpty && cliSupportsSessionFlag
            if narrow { for file in files { arguments += ["--session", file] } }
            do {
                var result = try await runCLI(arguments)
                if narrow, !result.succeeded, CLIOutput.isUnsupported(result.stderr) {
                    // An older CLI without `--session`: a full sync covers it.
                    cliSupportsSessionFlag = false
                    result = try await runCLI(["--json", "logs", "sync"])
                }
                if result.succeeded {
                    do {
                        let report = try CLIOutput.syncReport(result.stdout)
                        state.succeed(report, at: Date())
                        held = report.skippedNoToken
                        ledger.record(report, at: Date())
                        try? ledger.save(to: Self.ledgerURL)
                        notifyUploads(report)
                    } catch {
                        state.fail(message: "could not read the sync report")
                    }
                } else {
                    state.fail(stderr: result.stderr, status: result.status)
                }
            } catch {
                state.fail(message: error.localizedDescription)
            }
            for file in files { failedFiles[file] = state.errorMessage }
            if let message = state.errorMessage { notifyError(message) }
            await refreshProjects()
            NotificationCenter.default.post(name: .livingBrainSynced, object: nil)
            if !queuedFiles.isEmpty {
                let next = Array(queuedFiles)
                queuedFiles = []
                syncNow(files: next, userInitiated: false)
            }
        }
    }

    private func scheduleTimer() {
        timer?.invalidate()
        timer = nil
        guard autoSync else { return }
        // Tick every minute and sync when 15 minutes have passed: robust
        // across sleep, where a 15-minute timer would drift.
        timer = Timer.scheduledTimer(withTimeInterval: 60, repeats: true) { [weak self] _ in
            Task { @MainActor in
                guard let self, self.autoSync, !self.paused, !self.state.isSyncing else { return }
                if AutoSync.isDue(lastSyncAt: self.state.lastSyncAt, now: Date()) {
                    self.syncNow(userInitiated: false)
                }
            }
        }
    }

    // MARK: Live sync

    private func configureWatcher() {
        watcher?.stop()
        watcher = nil
        liveTimer?.invalidate()
        liveTimer = nil
        watchedRoots = []
        guard liveSync else { return }
        let watcher = SessionWatcher { [weak self] path in
            MainActor.assumeIsolated {
                guard let self else { return }
                self.debouncer.touch(path, at: Date())
                self.pendingFiles = self.debouncer.pending.count
            }
        }
        watchedRoots = watcher.start()
        self.watcher = watcher
        liveTimer = Timer.scheduledTimer(withTimeInterval: 15, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.flushQuietFiles() }
        }
    }

    private func flushQuietFiles() {
        let due = debouncer.takeDue(now: Date())
        pendingFiles = debouncer.pending.count
        guard !due.isEmpty, !paused else { return }
        // Only files that still exist; a deleted log has nothing to send.
        let files = due.filter { FileManager.default.fileExists(atPath: $0) }
        guard !files.isEmpty else { return }
        syncNow(files: files, userInitiated: false)
    }

    // MARK: Notifications

    private var canNotify: Bool { Bundle.main.bundleIdentifier != nil && notifications }

    private func requestNotificationPermission() {
        guard Bundle.main.bundleIdentifier != nil else { return }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    private func notifyUploads(_ report: SyncReport) {
        guard canNotify, let count = throttle.upload(count: report.uploaded, now: Date()) else { return }
        let projects = Set(report.sessions.filter { $0.status == "uploaded" }.map(\.projectName)).sorted()
        post(
            title: "Synced \(count) agent session\(count == 1 ? "" : "s")",
            body: projects.isEmpty ? "Living Brain is up to date." : projects.joined(separator: ", ")
        )
    }

    private func notifyError(_ message: String) {
        guard canNotify, throttle.error(message, now: Date()) else { return }
        post(title: "Living Brain sync failed", body: message)
    }

    private func post(title: String, body: String) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = Scrub.text(body)
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    // MARK: Projects

    func refreshProjects() async {
        do {
            let result = try await runCLI(["--json", "logs", "status"])
            if result.succeeded, let status = try? CLIOutput.allowStatus(result.stdout) {
                projects = status.projects
                projectsError = nil
                return
            }
        } catch {
            // Fall through to the file.
        }
        let path = AllowlistLocation.path(environment: ProcessInfo.processInfo.environment, home: NSHomeDirectory())
        do {
            projects = try AllowlistLocation.read(path: path).map { SyncedProject(path: $0) }
            projectsError = nil
        } catch {
            projectsError = "Could not read the opt-in file"
        }
    }

    func addProject() {
        NSApp.activate(ignoringOtherApps: true)
        let panel = NSOpenPanel()
        panel.canChooseDirectories = true
        panel.canChooseFiles = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Sync This Repo"
        panel.message = "Choose a repository whose agent sessions may sync to Living Brain."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        setAllowed(url.path, allow: true)
    }

    func removeProject(_ project: SyncedProject) {
        setAllowed(project.path, allow: false)
    }

    func setAllowed(_ path: String, allow: Bool) {
        Task {
            do {
                let result = try await runCLI(["--json", "logs", allow ? "allow" : "deny", path])
                if result.succeeded {
                    await refreshProjects()
                } else {
                    projectsError = CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)"
                }
            } catch {
                projectsError = error.localizedDescription
            }
            NotificationCenter.default.post(name: .livingBrainProjectsChanged, object: nil)
        }
    }

    /// `logs workspace <path> <id|default>`: send an allowed project's sessions
    /// to another workspace. The CLI refuses a project that is not opted in.
    func setWorkspace(_ project: SyncedProject, to workspace: String) {
        Task {
            do {
                let result = try await runCLI(["--json", "logs", "workspace", project.path, workspace])
                if result.succeeded {
                    await refreshProjects()
                } else {
                    projectsError = CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)"
                }
            } catch {
                projectsError = error.localizedDescription
            }
            NotificationCenter.default.post(name: .livingBrainProjectsChanged, object: nil)
        }
    }

    // MARK: Sign in

    func signIn() {
        guard !signingIn else { return }
        signingIn = true
        loginPrompt = nil
        let handle = CLIProcess()
        loginProcess = handle
        Task {
            do {
                let result = try await runCLI(["--json", "login"], onStderrLine: { line in
                    guard let prompt = CLIOutput.loginPrompt(line) else { return }
                    Task { @MainActor in
                        self.loginPrompt = prompt
                        if let url = prompt.openURL { NSWorkspace.shared.open(url) }
                    }
                }, handle: handle)
                if result.succeeded {
                    state.signedIn()
                    NotificationCenter.default.post(name: .livingBrainCLISignedIn, object: nil)
                    syncNow()
                } else if loginProcess != nil {
                    state.fail(stderr: result.stderr, status: result.status)
                }
            } catch {
                state.fail(message: error.localizedDescription)
            }
            signingIn = false
            loginPrompt = nil
            loginProcess = nil
        }
    }

    func cancelSignIn() {
        let handle = loginProcess
        loginProcess = nil
        handle?.terminate()
    }

    // MARK: Settings

    func chooseCLI() {
        NSApp.activate(ignoringOtherApps: true)
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.showsHiddenFiles = true
        panel.message = "Choose the livingbrain CLI binary or wrapper script."
        guard panel.runModal() == .OK, let url = panel.url else { return }
        defaults.set(url.path, forKey: Keys.cliPath)
        cliSupportsSessionFlag = true
        objectWillChange.send()
        Task { await refreshProjects() }
    }

    func resetCLI() {
        defaults.removeObject(forKey: Keys.cliPath)
        cliSupportsSessionFlag = true
        objectWillChange.send()
        Task { await refreshProjects() }
    }

    func setLaunchAtLogin(_ enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
            launchAtLoginError = nil
        } catch {
            launchAtLoginError = error.localizedDescription
        }
        launchAtLogin = SMAppService.mainApp.status == .enabled
    }

    func open(_ url: URL) {
        NSWorkspace.shared.open(url)
    }

    func reveal(path: String) {
        NSWorkspace.shared.selectFile(path, inFileViewerRootedAtPath: "")
    }
}

extension Notification.Name {
    static let livingBrainSynced = Notification.Name("LivingBrainSynced")
    static let livingBrainProjectsChanged = Notification.Name("LivingBrainProjectsChanged")
    static let livingBrainCLISignedIn = Notification.Name("LivingBrainCLISignedIn")
}
