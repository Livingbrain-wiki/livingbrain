import AppKit
import LivingBrainKit
import SwiftUI

/// The sidebar's destinations.
enum Destination: String, CaseIterable, Identifiable, Hashable {
    case wiki, ask, timeline, usage, sessions, projects, settings
    var id: String { rawValue }

    static let workspace: [Destination] = [.wiki, .ask, .timeline, .usage]
    static let local: [Destination] = [.sessions, .projects]

    var title: String {
        switch self {
        case .wiki: return "Wiki"
        case .ask: return "Ask"
        case .timeline: return "Agent Sessions"
        case .usage: return "Usage"
        case .sessions: return "Recent Sessions"
        case .projects: return "Projects & Sync"
        case .settings: return "Settings"
        }
    }

    var symbol: String {
        switch self {
        case .wiki: return "book.pages"
        case .ask: return "questionmark.bubble"
        case .timeline: return "clock.badge.checkmark"
        case .usage: return "chart.bar.xaxis"
        case .sessions: return "clock.arrow.circlepath"
        case .projects: return "folder.badge.gearshape"
        case .settings: return "gearshape"
        }
    }
}

/// Window-wide navigation, so one view can send the reader to another
/// (an answer's citation to its page, a session to its note).
@MainActor
final class Router: ObservableObject {
    @Published var destination: Destination
    /// A page the Wiki should open next.
    @Published var pendingSlug: String?
    /// Bumped by ⌘K so the Wiki focuses its search field.
    @Published var searchRequest = 0
    /// Bumped by ⌘N to open the new-note sheet.
    @Published var newNoteRequest = 0

    init() {
        let args = CommandLine.arguments
        if let i = args.firstIndex(of: "--destination"), i + 1 < args.count,
           let d = Destination(rawValue: args[i + 1])
        {
            destination = d
        } else {
            destination = .wiki
        }
        if let i = args.firstIndex(of: "--open-page"), i + 1 < args.count { pendingSlug = args[i + 1] }
    }

    func openPage(_ slug: String) {
        pendingSlug = slug
        destination = .wiki
    }
}

struct MainWindow: View {
    @ObservedObject var bar: BarModel
    @ObservedObject var sessions: SessionsModel
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var runner: RunnerModel
    @ObservedObject var router: Router

    var body: some View {
        NavigationSplitView {
            List(selection: Binding(get: { router.destination }, set: { if let d = $0 { router.destination = d } })) {
                Section("Workspace") {
                    ForEach(Destination.workspace) { item in
                        Label(item.title, systemImage: item.symbol).tag(item)
                    }
                }
                Section("This Mac") {
                    ForEach(Destination.local) { item in
                        Label(item.title, systemImage: item.symbol).tag(item)
                    }
                }
                Section {
                    Label(Destination.settings.title, systemImage: Destination.settings.symbol).tag(Destination.settings)
                }
            }
            .navigationSplitViewColumnWidth(min: 190, ideal: 220)
            .safeAreaInset(edge: .bottom) {
                VStack(alignment: .leading, spacing: 10) {
                    AccountFooter(workspace: workspace, router: router)
                    Divider()
                    SyncFooter(bar: bar)
                }
                .padding(10)
            }
        } detail: {
            detail
                // Each workspace's views start afresh: nothing read with one
                // token is shown under another.
                .id(workspace.activeId ?? "")
                .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .task(id: "\(workspace.activeId ?? "")|\(workspace.signedIn)") { await workspace.loadMe() }
        .tint(Theme.accent)
        .frame(minWidth: 900, minHeight: 560)
        .onAppear {
            // A window means a Dock icon; closing the last one puts the app
            // back in the menu bar only.
            NSApp.setActivationPolicy(.regular)
            NSApp.activate(ignoringOtherApps: true)
        }
        .onDisappear { NSApp.setActivationPolicy(.accessory) }
        .task { await Snapshot.takeIfRequested() }
    }

    @ViewBuilder private var detail: some View {
        switch router.destination {
        case .sessions: SessionsView(model: sessions)
        case .projects: ProjectsView(bar: bar, model: sessions)
        case .settings: SettingsView(workspace: workspace, bar: bar, runner: runner, router: router)
        case .wiki, .ask, .timeline, .usage:
            if workspace.signedIn {
                switch router.destination {
                case .wiki: WikiView(workspace: workspace, router: router)
                case .ask: AskView(workspace: workspace, router: router)
                case .timeline: TimelineView(workspace: workspace, router: router)
                default: UsageView(workspace: workspace)
                }
            } else {
                SignInView(workspace: workspace)
                    .navigationTitle(router.destination.title)
            }
        }
    }
}

/// The workspace in use under the sidebar, and the switcher between the
/// workspaces signed in on this Mac.
struct AccountFooter: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var router: Router

    var body: some View {
        if workspace.signedIn {
            Menu {
                WorkspaceMenuItems(workspace: workspace, router: router)
            } label: {
                HStack(spacing: 8) {
                    Image(systemName: "square.stack.3d.up.fill")
                        .foregroundStyle(Theme.accent)
                    VStack(alignment: .leading, spacing: 1) {
                        Text(workspace.activeName ?? "Signed in").font(.caption.weight(.medium)).lineLimit(1)
                        Text(workspace.baseURL.host ?? workspace.baseURL.absoluteString)
                            .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                    }
                    Spacer(minLength: 0)
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.caption2).foregroundStyle(.secondary)
                }
                .contentShape(Rectangle())
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .help("Switch workspace")
        } else {
            HStack(spacing: 8) {
                Image(systemName: "person.crop.circle.badge.questionmark")
                    .foregroundStyle(.secondary)
                VStack(alignment: .leading, spacing: 1) {
                    Text("Not signed in").font(.caption.weight(.medium))
                    Text(workspace.baseURL.host ?? workspace.baseURL.absoluteString)
                        .font(.caption2).foregroundStyle(.secondary).lineLimit(1)
                }
                Spacer()
                Button("Sign In") { router.destination = .settings; workspace.startSignIn() }
                    .controlSize(.small)
            }
        }
    }
}

/// The switcher's items: every signed-in workspace (the one in use ticked),
/// then adding another. Shared by the sidebar and the Workspace menu.
struct WorkspaceMenuItems: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var router: Router

    var body: some View {
        ForEach(workspace.workspaces) { item in
            Toggle(item.name.isEmpty ? item.id : item.name, isOn: Binding(
                get: { item.id == workspace.activeId },
                set: { if $0 { workspace.switchTo(item.id) } }
            ))
        }
        if workspace.signedIn && workspace.activeId == nil {
            Toggle(workspace.activeName ?? "Current workspace", isOn: .constant(true))
        }
        Divider()
        Button("Add Workspace…") {
            router.destination = .settings
            workspace.startSignIn()
        }
        Button("Manage Workspaces…") { router.destination = .settings }
    }
}

/// Sync status and a Sync button, pinned under the sidebar.
struct SyncFooter: View {
    @ObservedObject var bar: BarModel

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                Circle()
                    .fill(bar.state.errorMessage != nil ? Color.red : (bar.paused ? Color.orange : Theme.accent))
                    .frame(width: 7, height: 7)
                Text(bar.paused ? "Sync paused" : bar.state.lastSyncLine())
                    .font(.caption).foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            if let error = bar.state.errorMessage {
                Text(error).font(.caption2).foregroundStyle(.red).lineLimit(2)
            }
            Button {
                bar.syncNow()
            } label: {
                Label(bar.state.isSyncing ? "Syncing…" : "Sync Now", systemImage: "arrow.triangle.2.circlepath")
                    .frame(maxWidth: .infinity)
            }
            .disabled(bar.state.isSyncing)
            .controlSize(.small)
        }
    }
}

/// Shown in place of a workspace view until the app has a token.
struct SignInView: View {
    @ObservedObject var workspace: WorkspaceModel

    var body: some View {
        VStack(spacing: 18) {
            Image(systemName: "brain.head.profile").font(.system(size: 54)).foregroundStyle(Theme.accent)
            Text("Sign in to Living Brain").font(.title2.weight(.semibold))
            Text("Browse and edit the wiki, ask with citations, and see your synced agent sessions and usage.")
                .foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 420)
            if let problem = workspace.authProblem {
                Label(problem, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            SignInControls(workspace: workspace)
        }
        .padding(40)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct SignInControls: View {
    @ObservedObject var workspace: WorkspaceModel

    var body: some View {
        VStack(spacing: 10) {
            switch workspace.signIn {
            case let .waiting(code, url):
                VStack(spacing: 8) {
                    Text("Enter this code in the browser").foregroundStyle(.secondary)
                    Text(code).font(.system(.title, design: .monospaced).weight(.semibold)).textSelection(.enabled)
                    HStack {
                        ProgressView().controlSize(.small)
                        Text("Waiting for approval…").foregroundStyle(.secondary)
                    }
                    HStack {
                        if let url { Button("Open Page Again") { NSWorkspace.shared.open(url) } }
                        Button("Cancel") { workspace.cancelSignIn() }
                    }
                }
            default:
                HStack {
                    Button("Sign In with Browser") { workspace.startSignIn() }
                        .buttonStyle(.borderedProminent)
                        .keyboardShortcut(.defaultAction)
                    Button("Use the CLI's Sign-in") { workspace.importCLISignIn() }
                        .help("Copy the token `livingbrain login` saved in the keychain. macOS asks to allow this.")
                }
                if case let .failed(message) = workspace.signIn {
                    Text(message).font(.callout).foregroundStyle(.red).multilineTextAlignment(.center).frame(maxWidth: 440)
                }
            }
        }
    }
}
