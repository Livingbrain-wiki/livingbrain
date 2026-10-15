import AppKit
import LivingBrainKit
import SwiftUI

struct SessionsView: View {
    @ObservedObject var model: SessionsModel

    var body: some View {
        content
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .navigationTitle("Recent Sessions")
            .navigationSubtitle(subtitle)
            .searchable(text: $model.search, placement: .toolbar, prompt: "Filter sessions")
            .toolbar {
                ToolbarItemGroup {
                    Toggle(isOn: $model.optedInOnly) {
                        Label("Opted-in Only", systemImage: "checkmark.seal")
                    }
                    .help("Show only sessions from opted-in repositories")
                    Picker("Project", selection: $model.projectFilter) {
                        Text("All Projects").tag(String?.none)
                        Divider()
                        ForEach(model.projectNames, id: \.self) { Text($0).tag(String?.some($0)) }
                    }
                    .frame(minWidth: 150)
                    Button { model.reload() } label: {
                        Label("Reload", systemImage: "arrow.clockwise")
                    }
                    .disabled(model.load == .loading)
                    .help("Re-read local sessions (nothing is sent)")
                }
            }
            .onAppear { if model.load == .idle { model.reload() } }
    }

    private var subtitle: String {
        guard let at = model.loadedAt else { return "" }
        let shown = model.filteredRows.count
        return "\(shown) of \(model.rows.count) sessions · read \(at.formatted(date: .omitted, time: .shortened))"
    }

    @ViewBuilder private var content: some View {
        switch model.load {
        case .idle:
            StatePlaceholder(kind: .loading, title: "Reading local agent sessions…")
        case .loading where model.rows.isEmpty:
            StatePlaceholder(kind: .loading, title: "Reading local agent sessions…")
        case let .failed(message) where model.rows.isEmpty:
            StatePlaceholder(kind: .error, title: "Could not read sessions", message: message, retry: model.reload)
        default:
            if model.rows.isEmpty {
                StatePlaceholder(kind: .empty, title: "No agent sessions",
                                 message: "Claude Code and Codex sessions on this Mac appear here.",
                                 systemImage: "bubble.left.and.text.bubble.right")
            } else {
                HSplitView {
                    SessionTable(model: model)
                        .frame(minWidth: 520, idealWidth: 640, maxWidth: .infinity, maxHeight: .infinity)
                    SessionDetail(model: model)
                        .frame(minWidth: 320, idealWidth: 400, maxWidth: 560, maxHeight: .infinity)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}

enum RelativeTime {
    static let formatter: RelativeDateTimeFormatter = {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .short
        return f
    }()

    /// "12 min. ago", "just now".
    static func string(_ date: Date, now: Date = Date()) -> String {
        now.timeIntervalSince(date) < 60 ? "just now" : formatter.localizedString(for: date, relativeTo: now)
    }
}

struct SessionTable: View {
    @ObservedObject var model: SessionsModel

    var body: some View {
        Table(model.filteredRows, selection: $model.selection) {
            TableColumn("Repository") { row in
                Text(row.repoName)
                    .fontWeight(.medium)
                    .lineLimit(1)
                    .help(row.session.repo)
            }
            .width(min: 100, ideal: 150)
            TableColumn("Branch") { row in
                Text(row.branch ?? "—").foregroundStyle(.secondary).lineLimit(1).help(row.branch ?? "")
            }
            .width(min: 60, ideal: 110)
            TableColumn("Agent") { row in
                Text(row.session.agent == "codex" ? "Codex" : "Claude Code").lineLimit(1)
            }
            .width(min: 60, ideal: 90)
            TableColumn("Last Active") { row in
                if let date = row.lastActivity {
                    Text(RelativeTime.string(date))
                        .foregroundStyle(.secondary)
                        .help(date.formatted(date: .complete, time: .standard))
                } else {
                    Text("—").foregroundStyle(.secondary)
                }
            }
            .width(min: 70, ideal: 100)
            TableColumn("Turns") { row in
                Text("\(row.session.turns)").monospacedDigit().frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 44, ideal: 52)
            TableColumn("Tokens") { row in
                Text(row.session.totalTokens.compact).monospacedDigit().frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 50, ideal: 62)
            TableColumn("Cost") { row in
                Text(row.session.totals?.costUsd.map { $0.formatted(.currency(code: "USD")) } ?? "—")
                    .monospacedDigit().foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }
            .width(min: 44, ideal: 60)
            TableColumn("Status") { row in SyncBadge(row: row) }
                .width(min: 120, ideal: 128)
        }
        .tableStyle(.inset(alternatesRowBackgrounds: true))
        .contextMenu(forSelectionType: SessionRow.ID.self) { _ in
            Button("Sync This Session") { model.syncSelected() }
                .disabled(model.selectedRow?.session.isOptedIn != true)
            Button("Open in Living Brain") {
                if let row = model.selectedRow { model.bar.open(row.webURL(base: model.bar.serverURL)) }
            }
            Button("Reveal Log in Finder") {
                if let file = model.selectedRow?.file { model.bar.reveal(path: file) }
            }
        }
    }
}

struct SyncBadge: View {
    let row: SessionRow

    var body: some View {
        let (text, color, symbol): (String, Color, String) = {
            switch row.status {
            case .notOptedIn: return ("Not opted in", .secondary, "minus.circle")
            case .pending: return ("Pending", .orange, "clock.fill")
            case .synced: return ("Synced", Theme.accent, "checkmark.circle.fill")
            case .failed: return ("Error", .red, "exclamationmark.triangle.fill")
            }
        }()
        Label(text, systemImage: symbol)
            .font(.caption.weight(.medium))
            .foregroundStyle(color)
            .padding(.horizontal, 7).padding(.vertical, 2)
            .background(Capsule().fill(color.opacity(0.12)))
            .lineLimit(1)
            .help({
                switch row.status {
                case let .synced(date): return "Synced \(date.formatted(date: .abbreviated, time: .shortened))"
                case let .failed(message): return message
                case .pending: return "Opted in; uploads on the next sync"
                case .notOptedIn: return "This repository is not opted in, so this session never uploads"
                }
            }())
    }
}

struct SessionDetail: View {
    @ObservedObject var model: SessionsModel

    var body: some View {
        if let row = model.selectedRow {
            VStack(alignment: .leading, spacing: 0) {
                header(row).padding(16)
                Divider()
                transcript.frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        } else {
            StatePlaceholder(kind: .empty, title: "Select a session",
                             message: "See the redacted record that sync sends.",
                             systemImage: "text.bubble")
        }
    }

    @ViewBuilder private func header(_ row: SessionRow) -> some View {
        VStack(alignment: .leading, spacing: 8) {
            HStack {
                Text(row.repoName).font(.title2.weight(.semibold))
                Spacer()
                SyncBadge(row: row)
            }
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                GridRow { Text("Repo").foregroundStyle(.secondary); Text(row.session.repo).textSelection(.enabled) }
                GridRow { Text("Branch").foregroundStyle(.secondary); Text(row.branch ?? "—") }
                GridRow {
                    Text("Last active").foregroundStyle(.secondary)
                    Text(row.lastActivity.map { $0.formatted(date: .abbreviated, time: .shortened) } ?? "—")
                }
                GridRow {
                    Text("Models").foregroundStyle(.secondary)
                    Text(row.session.models.isEmpty ? "—" : row.session.models.joined(separator: ", "))
                }
                GridRow {
                    Text("Usage").foregroundStyle(.secondary)
                    Text(usage(row.session))
                }
                if case let .failed(message) = row.status {
                    GridRow {
                        Text("Error").foregroundStyle(.secondary)
                        Text(message).foregroundStyle(.red)
                    }
                }
                if case let .synced(date) = row.status {
                    GridRow {
                        Text("Synced").foregroundStyle(.secondary)
                        Text(date.formatted(date: .abbreviated, time: .shortened))
                    }
                }
            }
            .font(.callout)
            HStack {
                Button("Open in Living Brain") { model.bar.open(row.webURL(base: model.bar.serverURL)) }
                    .buttonStyle(.borderedProminent)
                Button("Sync This Session") { model.syncSelected() }
                    .disabled(!row.session.isOptedIn || model.bar.state.isSyncing)
                    .help(row.session.isOptedIn ? "Upload this session now" : "Opt this repo in under Projects & Sync first")
                Button {
                    if let file = row.file { model.bar.reveal(path: file) }
                } label: { Image(systemName: "folder") }
                    .help("Reveal the log file in Finder")
                    .disabled(row.file == nil)
            }
        }
    }

    private func usage(_ session: SyncedSession) -> String {
        var parts = ["\(session.turns) turns", "\(session.toolCalls) tool calls"]
        if let t = session.totals {
            parts.append("in \(t.inputTokens.compact) / out \(t.outputTokens.compact) tokens")
            if let cost = t.costUsd { parts.append(cost.formatted(.currency(code: "USD"))) }
        }
        return parts.joined(separator: " · ")
    }

    @ViewBuilder private var transcript: some View {
        switch model.transcript {
        case .none, .loading:
            StatePlaceholder(kind: .loading, title: "Building the redacted record…")
        case let .failed(message):
            StatePlaceholder(kind: .error, title: "No transcript", message: message)
        case let .loaded(shown):
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    HStack(spacing: 6) {
                        Image(systemName: "lock.shield")
                        Text(shown.redacted == 0
                             ? "Redacted on this Mac before upload — nothing needed removing."
                             : "Redacted on this Mac before upload — \(shown.redacted) secret(s) removed.")
                    }
                    .font(.caption).foregroundStyle(.secondary)
                    if let record = shown.displayRecord {
                        if record.truncated { Text("Truncated to fit the upload limit.").font(.caption).foregroundStyle(.secondary) }
                        ForEach(record.turns) { turn in TurnBubble(turn: turn) }
                    } else if let body = shown.body {
                        Text("Shown as the exact text sync sends (redaction left it unparseable as JSON).")
                            .font(.caption).foregroundStyle(.orange)
                        Text(body.count > 60_000 ? String(body.prefix(60_000)) + "…" : body)
                            .font(.system(.caption, design: .monospaced))
                            .textSelection(.enabled)
                    }
                }
                .padding(16)
            }
        }
    }
}

struct TurnBubble: View {
    let turn: SessionRecord.Turn

    var body: some View {
        let isUser = turn.role == "user"
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Label(isUser ? "You" : "Agent", systemImage: isUser ? "person.fill" : "cpu")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(isUser ? Theme.accent : .secondary)
                Spacer()
                if let at = turn.at.flatMap(ISODate.parse) {
                    Text(at.formatted(date: .omitted, time: .shortened)).font(.caption2).foregroundStyle(.tertiary)
                }
            }
            if !turn.text.isEmpty {
                Text(turn.text).textSelection(.enabled).frame(maxWidth: .infinity, alignment: .leading)
            }
            if !turn.toolCalls.isEmpty {
                FlowTools(names: turn.toolCalls)
            }
        }
        .padding(10)
        .background(RoundedRectangle(cornerRadius: 10).fill(isUser ? Theme.accent.opacity(0.08) : Color.secondary.opacity(0.07)))
    }
}

/// Tool names as small capsules (inputs and results never leave the Mac).
struct FlowTools: View {
    let names: [String]

    var body: some View {
        let counted = Dictionary(names.map { ($0, 1) }, uniquingKeysWith: +).sorted { $0.key < $1.key }
        HStack(spacing: 4) {
            Image(systemName: "wrench.and.screwdriver").font(.caption2).foregroundStyle(.secondary)
            ForEach(counted.prefix(8), id: \.key) { name, count in
                Text(count > 1 ? "\(name) ×\(count)" : name)
                    .font(.caption2.monospaced())
                    .padding(.horizontal, 6).padding(.vertical, 2)
                    .background(Capsule().fill(Color.secondary.opacity(0.12)))
            }
            if counted.count > 8 { Text("+\(counted.count - 8)").font(.caption2).foregroundStyle(.secondary) }
        }
    }
}
