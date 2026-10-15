import AppKit
import Foundation
import LivingBrainKit

/// The Recent Sessions browser and the Projects list: both read a local
/// `logs sync --dry-run` (nothing is sent) and the ledger of past syncs.
@MainActor
final class SessionsModel: ObservableObject {
    enum Load: Equatable {
        case idle, loading, loaded, failed(String)
    }

    enum Transcript: Equatable {
        case none, loading, loaded(SessionShow), failed(String)
    }

    @Published private(set) var load: Load = .idle
    @Published private(set) var rows: [SessionRow] = []
    @Published private(set) var repos: [RepoSummary] = []
    @Published private(set) var loadedAt: Date?
    @Published var projectFilter: String? = nil
    @Published var search = ""
    @Published var selection: SessionRow.ID? {
        didSet { if selection != oldValue { loadTranscript() } }
    }
    @Published private(set) var transcript: Transcript = .none

    let bar: BarModel
    private var report: SyncReport?
    private var observers: [NSObjectProtocol] = []

    init(bar: BarModel) {
        self.bar = bar
        for name in [Notification.Name.livingBrainSynced, .livingBrainProjectsChanged] {
            observers.append(NotificationCenter.default.addObserver(forName: name, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.refreshIfLoaded() }
            })
        }
    }

    var projectNames: [String] {
        Array(Set(rows.map(\.session.projectName))).sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }
    }

    var filteredRows: [SessionRow] {
        rows.filter { row in
            (projectFilter == nil || row.session.projectName == projectFilter)
                && (search.isEmpty
                    || row.session.repo.localizedCaseInsensitiveContains(search)
                    || (row.session.branch ?? "").localizedCaseInsensitiveContains(search)
                    || row.session.sessionId.localizedCaseInsensitiveContains(search)
                    || row.session.models.joined(separator: " ").localizedCaseInsensitiveContains(search))
        }
    }

    var selectedRow: SessionRow? { rows.first { $0.id == selection } }

    func refreshIfLoaded() {
        if load != .idle { reload() }
    }

    func reload() {
        guard load != .loading else { return }
        load = .loading
        Task {
            do {
                let result = try await bar.runCLI(["--json", "logs", "sync", "--dry-run"])
                guard result.succeeded else {
                    load = .failed(CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)")
                    return
                }
                let report = try CLIOutput.syncReport(result.stdout)
                self.report = report
                rows = SessionRow.rows(from: report, ledger: bar.ledger)
                repos = RepoSummary.summarise(
                    sessions: report.sessions,
                    allowed: bar.projects.map(\.path),
                    home: NSHomeDirectory()
                )
                loadedAt = Date()
                load = .loaded
            } catch {
                load = .failed(Scrub.text(error.localizedDescription))
            }
        }
    }

    private func loadTranscript() {
        guard let row = selectedRow, let file = row.session.file else {
            transcript = selectedRow == nil ? .none : .failed("Update the livingbrain CLI to see transcripts (it needs `logs show`).")
            return
        }
        transcript = .loading
        let id = row.id
        Task {
            do {
                let result = try await bar.runCLI(["--json", "logs", "show", file])
                guard selection == id else { return }
                if result.succeeded {
                    transcript = .loaded(try CLIOutput.sessionShow(result.stdout))
                } else if CLIOutput.isUnsupported(result.stderr) {
                    transcript = .failed("Update the livingbrain CLI to see transcripts (it needs `logs show`).")
                } else {
                    transcript = .failed(CLIOutput.firstErrorLine(result.stderr) ?? "livingbrain exited with status \(result.status)")
                }
            } catch {
                transcript = .failed(Scrub.text(error.localizedDescription))
            }
        }
    }

    func syncSelected() {
        guard let file = selectedRow?.session.file else { return }
        bar.syncNow(files: [file])
    }

    func setOptIn(_ repo: RepoSummary, _ allow: Bool) {
        bar.setAllowed(repo.path, allow: allow)
    }
}
