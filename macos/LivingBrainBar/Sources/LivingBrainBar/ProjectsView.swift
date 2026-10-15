import LivingBrainKit
import SwiftUI

struct ProjectsView: View {
    @ObservedObject var bar: BarModel
    @ObservedObject var model: SessionsModel

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
        }
        .formStyle(.grouped)
        .navigationTitle("Projects & Sync")
        .onAppear { if model.load == .idle { model.reload() } }
    }
}
