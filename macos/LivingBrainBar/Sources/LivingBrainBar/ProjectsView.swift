import LivingBrainKit
import SwiftUI

struct ProjectsView: View {
    @ObservedObject var bar: BarModel
    @ObservedObject var model: SessionsModel
    /// The signed-in workspaces, for each project's picker.
    @ObservedObject var accounts: WorkspaceModel

    var body: some View {
        Form {
            Section {
                Toggle(isOn: $bar.paused) {
                    Text("Pause all syncing")
                    Text("Stops live and scheduled sync. Sync Now still works.")
                }
                Toggle(isOn: $bar.liveSync) {
                    Text("Live sync")
                    Text(bar.watchedRoots.isEmpty
                         ? "Watches ~/.claude/projects and ~/.codex/sessions; syncs a session after 2 minutes without changes."
                         : "Watching \(bar.watchedRoots.map { SyncedProject(path: $0).displayPath() }.joined(separator: " and ")). \(bar.pendingFiles) session(s) waiting to go quiet.")
                }
                Toggle("Sync everything every 15 minutes", isOn: $bar.autoSync)
                Toggle(isOn: $bar.notifications) {
                    Text("Notifications")
                    Text("On upload and on error, at most one every few minutes.")
                }
            } header: {
                Text("Sync")
            }

            Section {
                if model.load == .loading && model.repos.isEmpty {
                    HStack { ProgressView().controlSize(.small); Text("Reading local sessions…").foregroundStyle(.secondary) }
                } else if model.repos.isEmpty {
                    Text("No repositories yet. Agent sessions on this Mac will list their repos here.")
                        .foregroundStyle(.secondary)
                }
                ForEach(model.repos) { repo in
                    VStack(alignment: .leading, spacing: 4) {
                        Toggle(isOn: Binding(get: { repo.optedIn }, set: { model.setOptIn(repo, $0) })) {
                            HStack {
                                Image(systemName: repo.optedIn ? "folder.fill" : "folder").foregroundStyle(repo.optedIn ? Theme.accent : .secondary)
                                VStack(alignment: .leading) {
                                    Text(repo.name)
                                    Text(SyncedProject(path: repo.path).displayPath()).font(.caption).foregroundStyle(.secondary)
                                }
                                Spacer()
                                Text("\(repo.sessions) session\(repo.sessions == 1 ? "" : "s")")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        .toggleStyle(.switch)
                        if repo.optedIn, let project = bar.projects.first(where: { $0.path == repo.path }) {
                            workspacePicker(for: project)
                        }
                    }
                }
                if let error = bar.projectsError {
                    Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.red)
                }
                Button("Add Project…") { bar.addProject() }
            } header: {
                Text("Projects")
            } footer: {
                Text("Only sessions whose every working directory sits inside an opted-in repository upload. The CLI enforces this and redacts locally before anything leaves the Mac.")
                    .font(.caption).foregroundStyle(.secondary)
            }

            if !bar.held.isEmpty {
                Section {
                    ForEach(bar.held, id: \.self) { held in
                        HStack {
                            Label(held.message, systemImage: held.denied ? "exclamationmark.triangle" : "person.crop.circle.badge.exclamationmark")
                            Spacer()
                            if !held.denied { Button("Sign In") { accounts.startSignIn() } }
                        }
                    }
                } header: {
                    Text("Waiting for a sign-in")
                } footer: {
                    Text("These sessions stay on this Mac until their workspace is signed in. Pick that workspace on the page that opens. They are never sent to another workspace.")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .navigationTitle("Projects & Sync")
        .onAppear { if model.load == .idle { model.reload() } }
    }

    /// Where one opted-in project's sessions go: Automatic (the routes decide),
    /// Default (pinned to the default, even over a route), or a signed-in
    /// workspace. A workspace this Mac is not signed in to keeps its place.
    @ViewBuilder
    private func workspacePicker(for project: SyncedProject) -> some View {
        Picker("Workspace", selection: Binding(
            get: { project.explicit ?? SyncedProject.automaticWorkspace },
            set: { bar.setWorkspace(project, to: $0) }
        )) {
            Text("Automatic").tag(SyncedProject.automaticWorkspace)
            Text("Default").tag(SyncedProject.defaultWorkspace)
            ForEach(accounts.workspaces) { item in
                Text(item.name).tag(item.id)
            }
            if let explicit = project.explicit, explicit != SyncedProject.defaultWorkspace,
               !accounts.workspaces.contains(where: { $0.id == explicit }) {
                Text("\(explicit) (not signed in)").tag(explicit)
            }
        }
        .pickerStyle(.menu)
        .font(.caption)
        if project.explicit == nil {
            hint(for: project)
        }
    }

    /// What decides an automatic project's sessions, and the action that fits.
    /// A parent's explicit choice is named as the parent. A held checkout offers
    /// a pin that never widens consent (see `SyncedProject.pinAction`). A folder
    /// of checkouts is warned that pinning it would override every route inside.
    @ViewBuilder
    private func hint(for project: SyncedProject) -> some View {
        switch project.effective {
        case SyncedProject.defaultWorkspace:
            EmptyView()
        case "per-repository":
            Text("Each repository inside this folder follows its own route. Pinning this folder would override every one of them.")
                .font(.caption).foregroundStyle(.secondary)
            ForEach(project.nested) { nested in
                HStack {
                    Text("\((nested.path as NSString).lastPathComponent) → \(nested.effective)")
                        .font(.caption2).foregroundStyle(.secondary)
                    if nested.effective == "unroutable" {
                        pinMenu(project, checkout: nested.path, title: "Pin \((nested.path as NSString).lastPathComponent)")
                    }
                }
            }
        case "unroutable":
            Text("Held: \(project.routeReason ?? "a route cannot read this repository").")
                .font(.caption).foregroundStyle(.secondary)
            pinMenu(project, checkout: project.repoTop ?? project.path, title: nil)
        default:
            if let parent = project.inheritedFrom {
                Text("Sent to \(project.effective) from \(parent).")
                    .font(.caption).foregroundStyle(.secondary)
            } else {
                Text("Sent to \(project.effective) by route \(project.routeReason ?? "").")
                    .font(.caption).foregroundStyle(.secondary)
            }
        }
    }

    /// The pin menu for one checkout. Offered as "Pin This Repository" when the
    /// checkout may be pinned; when it sits above the allowed path (never pinned
    /// without widening consent), the menu pins the allowed folder instead.
    @ViewBuilder
    private func pinMenu(_ project: SyncedProject, checkout: String, title: String?) -> some View {
        let refused = project.pinAction(for: checkout) == .refused
        let target = refused ? project.path : checkout
        Menu(title ?? (refused ? "Pin This Folder" : "Pin This Repository")) {
            Button("Default") { bar.pin(project, checkout: target, to: SyncedProject.defaultWorkspace) }
            ForEach(accounts.workspaces) { item in
                Button(item.name) { bar.pin(project, checkout: target, to: item.id) }
            }
        }
        .font(.caption)
    }
}
