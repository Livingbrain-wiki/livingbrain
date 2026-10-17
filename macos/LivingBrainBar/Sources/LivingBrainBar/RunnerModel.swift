import AppKit
import Combine
import Foundation
import LivingBrainKit

/// "Run brain jobs with my subscription": keeps one `livingbrain jobs run`
/// child alive while the setting is on, and tells the server to route this
/// user's model jobs (Main) to this device.
///
/// Turning it **on** starts the runner and sends `PUT /v1/models/mine/main`.
/// Turning it **off** stops the runner and sends `DELETE /v1/models/mine/main`
/// — the least surprising choice: otherwise the server would keep queueing
/// jobs for a device that no longer answers them, and they would stall until
/// the setting came back. **Pause Syncing** only stops the child; the route
/// stays, so jobs wait (their leases lapse) and run when syncing resumes.
@MainActor
final class RunnerModel: ObservableObject {
    enum Keys {
        static let enabled = "runner.enabled"
        static let agent = "runner.agent"
        static let model = "runner.model"
    }

    @Published private(set) var activity = RunnerActivity()
    @Published private(set) var agents: AgentsReport?
    @Published private(set) var agentsError: String?
    /// The last server call's failure (routing the jobs here, or undoing it).
    @Published private(set) var routeError: String?
    @Published private(set) var server: RunnerConnection?

    @Published var enabled: Bool {
        didSet {
            guard enabled != oldValue else { return }
            defaults.set(enabled, forKey: Keys.enabled)
            Task { await syncRoute() }
            reconcile()
        }
    }
    @Published var agent: RunnerAgent {
        didSet {
            guard agent != oldValue else { return }
            defaults.set(agent.rawValue, forKey: Keys.agent)
            if !agent.models.contains(model) { model = agent.defaultModel }
            restart()
            if enabled { Task { await syncRoute() } }
        }
    }
    @Published var model: String {
        didSet {
            guard model != oldValue else { return }
            defaults.set(model, forKey: Keys.model)
            if enabled { Task { await syncRoute() } }
        }
    }

    private let bar: BarModel
    private let workspace: WorkspaceModel
    private let defaults = UserDefaults.standard
    private var process: RunnerProcess?
    private var startedAt = Date()
    private var backoff = RunnerBackoff()
    private var restartTask: Task<Void, Never>?
    private var pausedSink: AnyCancellable?
    private var quitObserver: Any?

    init(bar: BarModel, workspace: WorkspaceModel) {
        self.bar = bar
        self.workspace = workspace
        let agent = RunnerAgent(rawValue: defaults.string(forKey: Keys.agent) ?? "") ?? .claude
        self.agent = agent
        self.model = defaults.string(forKey: Keys.model) ?? agent.defaultModel
        self.enabled = defaults.bool(forKey: Keys.enabled)
        Task { await refreshAgents() }
        reconcile()
        // Pause Syncing stops the child too; resuming starts it again.
        pausedSink = bar.$paused.dropFirst().removeDuplicates().sink { [weak self] _ in
            DispatchQueue.main.async { self?.reconcile() }
        }
        quitObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.shutdown() }
        }
    }

    /// The menu's lines about the runner.
    func lines(now: Date = Date()) -> [String] {
        activity.lines(agent: agent, now: now)
    }

    var agentStatus: AgentStatus? { agents?.status(agent) }

    // MARK: Process

    /// Start or stop the child to match the setting and Pause.
    func reconcile() {
        let wanted = enabled && !bar.paused
        if wanted, process == nil, restartTask == nil {
            start()
        } else if !wanted {
            restartTask?.cancel()
            restartTask = nil
            stop()
        }
    }

    func restart() {
        guard process != nil else { return }
        stop()
        backoff.reset()
        reconcile()
    }

    /// Quit: stop the child; the server route is left as it is.
    func shutdown() {
        restartTask?.cancel()
        restartTask = nil
        stop()
    }

    private func start() {
        guard let path = bar.cliPath else {
            activity.exited(message: CLIRunError.notFound.errorDescription)
            return
        }
        let child = RunnerProcess()
        process = child
        startedAt = Date()
        do {
            try child.start(
                executable: path,
                arguments: JobsCommand.run(agent: agent),
                environment: CLIEnvironment.make(base: ProcessInfo.processInfo.environment,
                                                 apiURL: bar.serverURL.absoluteString,
                                                 handoff: bar.cliTokenHandoff()),
                onLine: { [weak self] line in
                    guard let event = RunnerEvent.parse(line) else { return }
                    Task { @MainActor in self?.activity.apply(event, at: Date()) }
                },
                onExit: { [weak self] status, message in
                    Task { @MainActor in self?.exited(child, status: status, message: message) }
                }
            )
        } catch {
            process = nil
            activity.exited(message: error.localizedDescription)
            scheduleRestart()
        }
    }

    private func stop() {
        let child = process
        process = nil
        child?.stop()
        activity.exited(message: nil)
    }

    private func exited(_ child: RunnerProcess, status: Int32, message: String?) {
        guard process === child else { return } // stopped on purpose
        process = nil
        let reason: String? = status == 2
            ? "Sign in to Living Brain first (the CLI is not logged in)"
            : (status == 0 ? nil : message ?? "livingbrain jobs run exited with status \(status)")
        activity.exited(message: reason)
        scheduleRestart()
    }

    private func scheduleRestart() {
        guard enabled, !bar.paused else { return }
        let delay = backoff.next(ranFor: Date().timeIntervalSince(startedAt))
        restartTask = Task { [weak self] in
            try? await Task.sleep(nanoseconds: UInt64(delay * 1_000_000_000))
            guard !Task.isCancelled else { return }
            await MainActor.run {
                self?.restartTask = nil
                self?.reconcile()
            }
        }
    }

    // MARK: Agents and server

    func refreshAgents() async {
        do {
            let result = try await bar.runCLI(JobsCommand.agents())
            if result.succeeded {
                agents = try AgentsReport.parse(result.stdout)
                agentsError = nil
            } else if CLIOutput.isUnsupported(result.stderr) {
                agentsError = "This livingbrain CLI is too old to run jobs — update it"
            } else {
                agentsError = CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)"
            }
        } catch {
            agentsError = error.localizedDescription
        }
    }

    /// Tell the server where this user's Main jobs run.
    func syncRoute() async {
        guard workspace.signedIn else {
            routeError = enabled ? "Sign in to the workspace (Settings) so jobs are routed to this Mac" : nil
            return
        }
        do {
            if enabled {
                server = try await workspace.call { [agent, model] in
                    try await $0.setRunner(role: .main, agent: agent, model: model)
                }
            } else {
                try await workspace.call { try await $0.removeRunner(role: .main) }
                server = nil
            }
            routeError = nil
        } catch let error as APIError {
            if case .notFound = error, !enabled { server = nil; routeError = nil; return }
            routeError = error.localizedDescription
        } catch {
            routeError = error.localizedDescription
        }
    }

    func refreshServer() async {
        guard workspace.signedIn else { return }
        server = (try? await workspace.call { try await $0.myRunners() })?.first { $0.role == ModelRole.main.rawValue }
    }
}
