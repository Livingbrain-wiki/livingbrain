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
                    ForEach(bar.held, id: \.workspace) { held in
                        HStack {
                            Label(held.message, systemImage: "person.crop.circle.badge.exclamationmark")
                            Spacer()
                            Button("Sign In") { accounts.startSignIn() }
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

    /// Where one opted-in project's sessions go. Default, then each signed-in
    /// workspace; a workspace this Mac is not signed in to keeps its place so
    /// the choice still reads.
    @ViewBuilder
    private func workspacePicker(for project: SyncedProject) -> some View {
        Picker("Workspace", selection: Binding(
            get: { project.workspace },
            set: { bar.setWorkspace(project, to: $0) }
        )) {
            Text("Default").tag(SyncedProject.defaultWorkspace)
            ForEach(accounts.workspaces) { item in
                Text(item.name).tag(item.id)
            }
            if project.workspace != SyncedProject.defaultWorkspace,
               !accounts.workspaces.contains(where: { $0.id == project.workspace }) {
                Text("\(project.workspace) (not signed in)").tag(project.workspace)
            }
        }
        .pickerStyle(.menu)
        .font(.caption)
        if let routed = project.routed, project.workspace == SyncedProject.defaultWorkspace {
            Text("Routed to \(routed) by a rule in the opt-in file. Choose a workspace to override it.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }
}
