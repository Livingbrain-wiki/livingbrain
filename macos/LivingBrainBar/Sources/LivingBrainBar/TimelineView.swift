import AppKit
import LivingBrainKit
import SwiftUI

/// Synced agent sessions per project, from the server:
/// `/v1/sources/projects?kind=agent_log`, `/v1/sources?kind=agent_log`,
/// `/v1/sources/{id}`.
@MainActor
final class TimelineModel: ObservableObject {
    enum Load: Equatable { case idle, loading, loaded, failed(String) }

    @Published private(set) var projects: [SourceProject] = []
    @Published var project = "" { didSet { if project != oldValue { reload() } } }
    @Published private(set) var sources: [SourceSummary] = []
    @Published private(set) var nextCursor: String?
    @Published private(set) var load: Load = .idle
    @Published var selection: SourceSummary.ID? { didSet { if selection != oldValue { openSelected() } } }
    @Published private(set) var detail: SourceDetail?
    @Published private(set) var detailError: String?
    @Published private(set) var detailLoading = false

    let workspace: WorkspaceModel

    init(workspace: WorkspaceModel) {
        self.workspace = workspace
    }

    func start() {
        guard load == .idle else { return }
        Task {
            projects = (try? await workspace.call { try await $0.sourceProjects(kind: "agent_log") }) ?? []
        }
        reload()
    }

    func reload() {
        load = .loading
        sources = []
        nextCursor = nil
        selection = nil
        fetch(cursor: nil)
    }

    func loadMore() {
        guard let cursor = nextCursor, load != .loading else { return }
        load = .loading
        fetch(cursor: cursor)
    }

    private func fetch(cursor: String?) {
        let project = self.project
        Task {
            do {
                let page = try await workspace.call { try await $0.sources(kind: "agent_log", project: project, limit: 50, cursor: cursor) }
                guard project == self.project else { return }
                sources += page.sources
                nextCursor = page.nextCursor
                load = .loaded
                if selection == nil, CommandLine.arguments.contains("--select-first") { selection = sources.first?.id }
            } catch {
                load = .failed(error.localizedDescription)
            }
        }
    }

    /// Rows grouped by local calendar day, newest first.
    var days: [(day: Date, rows: [SourceSummary])] {
        let calendar = Calendar.current
        let grouped = Dictionary(grouping: sources) { calendar.startOfDay(for: $0.createdDate ?? .distantPast) }
        return grouped.keys.sorted(by: >).map { ($0, grouped[$0]!) }
    }

    private func openSelected() {
        detail = nil
        detailError = nil
        guard let id = selection else { return }
        detailLoading = true
        Task {
            do {
                let loaded = try await workspace.call { try await $0.source(id) }
                if selection == id { detail = loaded }
            } catch {
                if selection == id { detailError = error.localizedDescription }
            }
            detailLoading = false
        }
    }
}

struct TimelineView: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var router: Router
    @StateObject private var model: TimelineModel

    init(workspace: WorkspaceModel, router: Router) {
        self.workspace = workspace
        self.router = router
        _model = StateObject(wrappedValue: TimelineModel(workspace: workspace))
    }

    var body: some View {
        HSplitView {
            list.frame(minWidth: 320, idealWidth: 380, maxWidth: 520, maxHeight: .infinity)
            detail.frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle("Agent Sessions")
        .navigationSubtitle(model.project.isEmpty ? "All projects" : model.project)
        .toolbar {
            ToolbarItemGroup {
                Picker(selection: $model.project) {
                    Text("All Projects").tag("")
                    if !model.projects.isEmpty { Divider() }
                    ForEach(model.projects) { p in Text("\(p.project)  (\(p.sources))").tag(p.project) }
                } label: { Label("Project", systemImage: "folder") }
                    .frame(minWidth: 170)
                Button { model.reload() } label: { Label("Reload", systemImage: "arrow.clockwise") }
                Button { NSWorkspace.shared.open(webURL) } label: { Label("Open in Browser", systemImage: "safari") }
                    .help("Open this list in Living Brain on the web")
            }
        }
        .onAppear { model.start() }
    }

    private var webURL: URL {
        var c = URLComponents(url: workspace.baseURL.appendingPathComponent("wiki.html"), resolvingAgainstBaseURL: false)!
        c.queryItems = [URLQueryItem(name: "view", value: "sessions")]
            + (model.project.isEmpty ? [] : [URLQueryItem(name: "project", value: model.project)])
        return c.url!
    }

    @ViewBuilder private var list: some View {
        if model.sources.isEmpty {
            switch model.load {
            case .idle, .loading: StatePlaceholder(kind: .loading, title: "Loading synced sessions…")
            case let .failed(message): StatePlaceholder(kind: .error, title: "Could not load sessions", message: message, retry: model.reload)
            case .loaded:
                StatePlaceholder(kind: .empty, title: "No synced sessions",
                                 message: "Opt a repo in under Projects & Sync and its sessions land here.",
                                 systemImage: "clock.badge.checkmark")
            }
        } else {
            List(selection: $model.selection) {
                ForEach(model.days, id: \.day) { group in
                    Section(group.day.formatted(date: .complete, time: .omitted)) {
                        ForEach(group.rows) { source in
                            VStack(alignment: .leading, spacing: 2) {
                                HStack {
                                    Text(source.project ?? "no project").fontWeight(.medium)
                                    Spacer()
                                    if let d = source.createdDate {
                                        Text(d.formatted(date: .omitted, time: .shortened)).font(.caption).foregroundStyle(.secondary)
                                    }
                                }
                                Text(source.path ?? source.id).font(.caption).foregroundStyle(.secondary).lineLimit(1)
                            }
                            .tag(source.id)
                        }
                    }
                }
                if model.nextCursor != nil {
                    Button(model.load == .loading ? "Loading…" : "Load More") { model.loadMore() }
                        .disabled(model.load == .loading)
                }
            }
        }
    }

    @ViewBuilder private var detail: some View {
        if model.selection == nil {
            StatePlaceholder(kind: .empty, title: "Select a session", message: "Its redacted record, as the server keeps it.",
                             systemImage: "text.bubble")
        } else if model.detailLoading {
            StatePlaceholder(kind: .loading, title: "Opening…")
        } else if let error = model.detailError {
            StatePlaceholder(kind: .error, title: "Could not open the session", message: error)
        } else if let detail = model.detail {
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if let record = detail.sessionRecord {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(detail.project ?? record.repo).font(.title2.weight(.semibold))
                            Text([record.agent == "codex" ? "Codex" : "Claude Code", record.branch, record.models.joined(separator: ", "),
                                  "\(record.turns.count) turn\(record.turns.count == 1 ? "" : "s")"].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · "))
                                .font(.callout).foregroundStyle(.secondary)
                        }
                        .padding(.bottom, 6)
                        ForEach(record.turns) { TurnBubble(turn: $0) }
                    } else {
                        Text(detail.body).font(.system(.callout, design: .monospaced)).textSelection(.enabled)
                    }
                }
                .padding(20)
            }
        }
    }
}
