import AppKit
import LivingBrainKit
import SwiftUI

/// The sidebar's destinations.
enum Destination: String, CaseIterable, Identifiable, Hashable {
    case sessions, projects
    var id: String { rawValue }

    var title: String {
        switch self {
        case .sessions: return "Recent Sessions"
        case .projects: return "Projects & Sync"
        }
    }

    var symbol: String {
        switch self {
        case .sessions: return "clock.arrow.circlepath"
        case .projects: return "folder.badge.gearshape"
        }
    }
}

struct MainWindow: View {
    @ObservedObject var bar: BarModel
    @ObservedObject var sessions: SessionsModel
    @SceneStorage("destination") private var destination: Destination = .sessions

    var body: some View {
        NavigationSplitView {
            List(selection: Binding(get: { destination }, set: { if let d = $0 { destination = d } })) {
                Section("Local") {
                    ForEach(Destination.allCases) { item in
                        Label(item.title, systemImage: item.symbol).tag(item)
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 180, ideal: 210)
            .safeAreaInset(edge: .bottom) { SyncFooter(bar: bar).padding(10) }
        } detail: {
            switch destination {
            case .sessions: SessionsView(model: sessions)
            case .projects: ProjectsView(bar: bar, model: sessions)
            }
        }
        .tint(Theme.accent)
        .frame(minWidth: 860, minHeight: 520)
        .onAppear {
            // A window means a Dock icon; closing the last one puts the app
            // back in the menu bar only.
            NSApp.setActivationPolicy(.regular)
            NSApp.activate(ignoringOtherApps: true)
        }
        .onDisappear { NSApp.setActivationPolicy(.accessory) }
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
