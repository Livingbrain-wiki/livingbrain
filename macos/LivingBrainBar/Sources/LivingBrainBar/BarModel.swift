import AppKit
import Foundation
import LivingBrainKit
import ServiceManagement

/// The menu bar extra's model: runs the CLI, keeps `SyncState`, and owns the
/// settings that live in `UserDefaults`.
@MainActor
final class BarModel: ObservableObject {
    static let appURL = URL(string: "https://app.livingbrain.wiki/")!
    static let sessionsURL = URL(string: "https://app.livingbrain.wiki/wiki.html?view=sessions")!

    enum Keys {
        static let autoSync = "autoSync"
        static let cliPath = "cliPath"
    }

    @Published private(set) var state = SyncState()
    @Published private(set) var projects: [SyncedProject] = []
    @Published private(set) var projectsError: String?
    @Published private(set) var loginPrompt: LoginPrompt?
    @Published private(set) var signingIn = false
    @Published private(set) var launchAtLogin = false
    @Published private(set) var launchAtLoginError: String?
    @Published var autoSync: Bool {
        didSet {
            UserDefaults.standard.set(autoSync, forKey: Keys.autoSync)
            scheduleTimer()
        }
    }

    private var timer: Timer?
    private var loginProcess: CLIProcess?
    private let defaults = UserDefaults.standard

    init() {
        autoSync = UserDefaults.standard.bool(forKey: Keys.autoSync)
        launchAtLogin = SMAppService.mainApp.status == .enabled
        scheduleTimer()
        Task { await refreshProjects() }
    }

    // MARK: CLI

    var cliPath: String? {
        CLILocator.resolve(configured: defaults.string(forKey: Keys.cliPath))
    }

    var cliPathLabel: String {
        guard let path = cliPath else { return "CLI: not found" }
        return "CLI: " + SyncedProject(path: path).displayPath()
    }

    private func runCLI(_ arguments: [String], onStderrLine: (@Sendable (String) -> Void)? = nil,
                        handle: CLIProcess = CLIProcess()) async throws -> CLIResult
    {
        guard let path = cliPath else { throw CLIRunError.notFound }
        return try await CLIRunner.run(
            executable: path,
            arguments: arguments,
            environment: CLIEnvironment.make(base: ProcessInfo.processInfo.environment),
            handle: handle,
            onStderrLine: onStderrLine
        )
    }

    // MARK: Sync

    func syncNow() {
        guard state.begin() == true else { return }
        objectWillChange.send()
        Task {
            do {
                // The CLI decides what may upload: a session outside an
                // opted-in repo never leaves the machine. This app only asks.
                let result = try await runCLI(["--json", "logs", "sync"])
                if result.succeeded {
                    do {
                        state.succeed(try CLIOutput.syncReport(result.stdout), at: Date())
                    } catch {
                        state.fail(message: "could not read the sync report")
                    }
                } else {
                    state.fail(stderr: result.stderr, status: result.status)
                }
            } catch {
                state.fail(message: error.localizedDescription)
            }
            await refreshProjects()
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
                guard let self, self.autoSync, !self.state.isSyncing else { return }
                if AutoSync.isDue(lastSyncAt: self.state.lastSyncAt, now: Date()) {
                    self.syncNow()
                }
            }
        }
    }

    // MARK: Projects

    func refreshProjects() async {
        do {
            let result = try await runCLI(["--json", "logs", "status"])
            if result.succeeded, let status = try? CLIOutput.allowStatus(result.stdout) {
                projects = status.allowed.map(SyncedProject.init)
                projectsError = nil
                return
            }
        } catch {
            // Fall through to the file.
        }
        let path = AllowlistLocation.path(environment: ProcessInfo.processInfo.environment, home: NSHomeDirectory())
        do {
            projects = try AllowlistLocation.read(path: path).map(SyncedProject.init)
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

    private func setAllowed(_ path: String, allow: Bool) {
        Task {
            do {
                let result = try await runCLI(["--json", "logs", allow ? "allow" : "deny", path])
                if result.succeeded, let status = try? CLIOutput.allowStatus(result.stdout) {
                    projects = status.allowed.map(SyncedProject.init)
                    projectsError = nil
                } else if !result.succeeded {
                    projectsError = CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)"
                }
            } catch {
                projectsError = error.localizedDescription
            }
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
        objectWillChange.send()
        Task { await refreshProjects() }
    }

    func resetCLI() {
        defaults.removeObject(forKey: Keys.cliPath)
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

    func reveal(_ project: SyncedProject) {
        NSWorkspace.shared.selectFile(nil, inFileViewerRootedAtPath: project.path)
    }
}
