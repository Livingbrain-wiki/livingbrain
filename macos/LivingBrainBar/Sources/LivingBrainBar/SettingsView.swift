import AppKit
import LivingBrainKit
import SwiftUI

struct SettingsView: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var bar: BarModel
    @ObservedObject var runner: RunnerModel
    @ObservedObject var router: Router
    @ObservedObject var updates: UpdateModel
    @State private var models: [ModelConnection] = []
    @State private var modelsError: String?
    @State private var tokens: [TokenSummary] = []
    @State private var tokensError: String?
    @State private var editingRole: ModelRole?
    @State private var catalog: ProviderCatalog?
    @State private var serverDraft = ""
    @State private var customModel = ""

    private var webSettings: URL { workspace.baseURL.appendingPathComponent("settings.html") }

    var body: some View {
        Form {
            account
            if workspace.signedIn {
                workspaceSection
                modelsSection
                tokensSection
            }
            runnerSection
            slackSection
            syncSection
            updatesSection
        }
        .formStyle(.grouped)
        .navigationTitle("Settings")
        .task(id: workspace.signedIn) { await reload() }
        .onAppear { serverDraft = bar.serverURLText }
        .sheet(item: $editingRole) { role in
            ModelEditor(role: role, current: models.first { $0.role == role.rawValue }, catalog: catalog,
                        workspace: workspace) { await reload() }
        }
    }

    // MARK: Sections

    private var account: some View {
        Section("Account") {
            LabeledContent("Server") {
                HStack {
                    TextField("Server", text: $serverDraft, prompt: Text(CLIEnvironment.defaultAPIURL))
                        .labelsHidden()
                        .textFieldStyle(.roundedBorder)
                        .frame(minWidth: 260)
                    Button("Apply") {
                        bar.serverURLText = serverDraft
                        workspace.serverChanged()
                    }
                    .disabled(serverDraft == bar.serverURLText)
                }
            }
            LabeledContent("Status") {
                if workspace.signedIn {
                    HStack {
                        Label("Signed in", systemImage: "checkmark.seal.fill").foregroundStyle(Theme.accent)
                        Button(workspace.activeName.map { "Sign Out of \($0)" } ?? "Sign Out") { workspace.signOut() }
                    }
                } else {
                    Text("Not signed in").foregroundStyle(.secondary)
                }
            }
            if !workspace.signedIn {
                SignInControls(workspace: workspace).frame(maxWidth: .infinity)
            }
            Text("The token is kept in your login keychain and only sent in the Authorization header to this server.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var workspaceSection: some View {
        Section("Workspace") {
            LabeledContent("Current") {
                Text(workspace.me?.workspaceLabel ?? workspace.workspaceId ?? "Not signed in")
                    .foregroundStyle(workspace.me == nil ? .secondary : .primary)
                    .textSelection(.enabled)
            }
            LabeledContent("Connected as") {
                Text(workspace.me?.principalLabel ?? "—")
                    .foregroundStyle(workspace.me == nil ? .secondary : .primary)
                    .textSelection(.enabled)
            }
            if !workspace.workspaces.isEmpty {
                Picker("Default workspace", selection: Binding(
                    get: { workspace.defaultId ?? "" },
                    set: { workspace.setDefault($0) }
                )) {
                    Text("Not chosen").tag("")
                    ForEach(workspace.workspaces) { item in
                        Text(item.name.isEmpty ? item.id : item.name).tag(item.id)
                    }
                }
                .help("The CLI's default destination (sessions without a repository workspace) uses this workspace's sign-in.")
            }
            ForEach(workspace.workspaces) { item in
                HStack {
                    Image(systemName: item.id == workspace.activeId ? "checkmark.circle.fill" : "circle")
                        .foregroundStyle(item.id == workspace.activeId ? Theme.accent : .secondary)
                    Text(item.name.isEmpty ? item.id : item.name)
                    Spacer()
                    if item.id != workspace.activeId {
                        Button("Switch") { workspace.switchTo(item.id) }
                    }
                    Button("Sign Out", role: .destructive) { workspace.remove(item.id) }
                }
            }
            LabeledContent("Add a workspace") {
                SignInControls(workspace: workspace)
            }
            Text("Each workspace has its own sign-in. Add Workspace opens the browser: pick the workspace on the approval page, then approve. Sync and the job runner use the default workspace's sign-in; each repository syncs to the workspace it names.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var modelsSection: some View {
        Section {
            if let modelsError {
                Label(modelsError, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            ForEach(ModelRole.allCases) { role in
                let connection = models.first { $0.role == role.rawValue }
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(role.title).fontWeight(.medium)
                        if let connection {
                            Text("\(connection.provider) · \(connection.model)\(connection.key.map { " · key \($0)" } ?? "")")
                                .font(.caption).foregroundStyle(.secondary)
                            if !connection.missing.isEmpty {
                                Text("Missing: \(connection.missing.joined(separator: ", "))").font(.caption).foregroundStyle(.orange)
                            }
                        } else {
                            Text(role == .main ? "Managed by Living Brain" : "Uses Main").font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    Spacer()
                    if let status = connection?.status {
                        Text(status).font(.caption)
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(Capsule().fill((status == "works" ? Theme.accent : Color.orange).opacity(0.15)))
                    }
                    Button(connection == nil ? "Connect…" : "Change…") { editingRole = role }
                    if connection != nil {
                        Button("Disconnect", role: .destructive) { disconnect(role) }
                    }
                }
            }
        } header: {
            Text("Models")
        } footer: {
            Text("Connecting or changing a model needs a workspace admin. Keys are sent once and never shown again.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    private var tokensSection: some View {
        Section {
            if let tokensError {
                Label(tokensError, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            } else if tokens.isEmpty {
                Text("No API tokens.").foregroundStyle(.secondary)
            }
            ForEach(tokens) { token in
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(token.name).fontWeight(.medium)
                        Text("\(token.prefix)… · \(token.scopeLabel)\(token.createdDate.map { " · created \($0.formatted(date: .abbreviated, time: .omitted))" } ?? "")")
                            .font(.caption.monospaced()).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Revoke", role: .destructive) { revoke(token) }
                }
            }
            Button("Create a Token in the Browser…") { NSWorkspace.shared.open(webSettings) }
        } header: {
            Text("API tokens")
        } footer: {
            Text("New tokens are minted from the web sign-in only, so a token can never create another; create them in the browser.")
                .font(.caption).foregroundStyle(.secondary)
        }
    }

    /// "Run brain jobs with my subscription": the subscription stays in
    /// Claude Code or Codex on this Mac; the brain only hands it the prompt.
    private var runnerSection: some View {
        Section {
            Toggle("Run brain jobs with my subscription", isOn: $runner.enabled)
            Picker("Agent", selection: $runner.agent) {
                ForEach(RunnerAgent.allCases) { agent in
                    Text("\(agent.title) — \(agent.subscription)").tag(agent)
                }
            }
            LabeledContent("Model") {
                HStack {
                    Picker("Model", selection: $runner.model) {
                        ForEach(runner.agent.models, id: \.self) { Text($0).tag($0) }
                        if !runner.agent.models.contains(runner.model) {
                            Text(runner.model.isEmpty ? "CLI default" : "\(runner.model) (custom)").tag(runner.model)
                        }
                    }
                    .labelsHidden()
                    .fixedSize()
                    TextField("Custom model id", text: $customModel)
                        .textFieldStyle(.roundedBorder)
                        .frame(minWidth: 140)
                        .onSubmit { useCustomModel() }
                    Button("Use") { useCustomModel() }
                        .disabled(customModel.trimmingCharacters(in: .whitespaces).isEmpty)
                }
            }
            ForEach(RunnerAgent.allCases) { agent in
                LabeledContent(agent.title) {
                    if let status = runner.agents?.status(agent) {
                        VStack(alignment: .trailing, spacing: 2) {
                            Text(status.summary)
                                .foregroundStyle(status.loggedIn ? Theme.accent : .secondary)
                            if let path = status.path {
                                Text([SyncedProject(path: path).displayPath(), status.version].compactMap { $0 }.joined(separator: " · "))
                                    .font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                            }
                        }
                    } else {
                        Text(runner.agentsError ?? "Checking…").foregroundStyle(.secondary)
                    }
                }
            }
            if runner.enabled {
                ForEach(runner.lines(), id: \.self) { Text($0).font(.callout) }
                if let server = runner.server {
                    Text("Main jobs route to this device: \(server.agent) · \(server.model.isEmpty ? "default model" : server.model)")
                        .font(.caption).foregroundStyle(.secondary)
                }
            }
            if let error = runner.routeError {
                Label(error, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            Button("Check Agents Again") { Task { await runner.refreshAgents() } }
        } header: {
            Text("Brain jobs on this Mac")
        } footer: {
            Text("Your Claude or ChatGPT subscription can only be used inside Claude Code or Codex, so the brain never holds it: while this is on, your own CLI on this Mac answers your brain's model jobs (one at a time, with tools off and a read-only sandbox). Turning it off sends your jobs back to the workspace's model. Prompts and answers are never logged.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .task { await runner.refreshServer() }
    }

    private func useCustomModel() {
        let id = customModel.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !id.isEmpty else { return }
        runner.model = id
        customModel = ""
    }

    private var slackSection: some View {
        Section("Slack") {
            LabeledContent("Connection") {
                Text("Not reported by the API").foregroundStyle(.secondary)
            }
            Button("Manage Slack in the Browser…") { NSWorkspace.shared.open(webSettings) }
        }
    }

    private var syncSection: some View {
        Section("Sync on this Mac") {
            Toggle("Pause all syncing", isOn: $bar.paused)
            Toggle("Live sync", isOn: $bar.liveSync)
            Toggle("Sync everything every 15 minutes", isOn: $bar.autoSync)
            Toggle("Notifications", isOn: $bar.notifications)
            LabeledContent("CLI") {
                HStack {
                    Text(bar.cliPath.map { SyncedProject(path: $0).displayPath() } ?? "not found")
                        .font(.callout.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                    Button("Choose…") { bar.chooseCLI() }
                    Button("Default") { bar.resetCLI() }
                }
            }
            Toggle("Launch at login", isOn: Binding(get: { bar.launchAtLogin }, set: { bar.setLaunchAtLogin($0) }))
            Button("Projects & Sync…") { router.destination = .projects }
        }
    }

    private var updatesSection: some View {
        Section("Updates") {
            LabeledContent("Current version", value: updates.currentVersion)
            Toggle("Automatically check for updates", isOn: $updates.autoCheck)
            LabeledContent("Last checked") {
                if let checked = updates.lastChecked {
                    Text(checked, style: .relative).foregroundStyle(.secondary)
                } else {
                    Text("Never").foregroundStyle(.secondary)
                }
            }
            if let newer = updates.newerRelease {
                updateDetails(newer)
            }
            updateStatus
            Button("Check for Updates") { updates.checkForUpdates() }
                .disabled(updates.isBusy)
        }
    }

    @ViewBuilder private func updateDetails(_ newer: Release) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Version \(newer.version.description) is available")
                .font(.headline)
            if updates.skippedVersion == newer.version.description {
                Text("You skipped this version. It will not be offered again; you can still install it here.")
                    .font(.callout).foregroundStyle(.secondary)
            }
            let notes = releaseNotesPreview(newer.notes)
            if !notes.isEmpty {
                Text(notes).font(.callout).foregroundStyle(.secondary).textSelection(.enabled)
            }
        }
        if case let .downloading(fraction) = updates.state {
            ProgressView(value: fraction) { Text("Downloading…") }
        }
        if updates.canInstallInPlace {
            HStack {
                Button("Install and Relaunch") { updates.installUpdate() }
                    .disabled(updates.isBusy)
                Button("Skip This Version") { updates.skip(newer) }
                    .disabled(updates.isBusy)
                Button("Release notes") { updates.openReleasePage() }
            }
        } else {
            Text("Move Living Brain to Applications to update.")
                .font(.callout).foregroundStyle(.secondary)
            Button("Release notes") { updates.openReleasePage() }
        }
    }

    @ViewBuilder private var updateStatus: some View {
        switch updates.state {
        case .checking: Text("Checking for updates…").foregroundStyle(.secondary)
        case .upToDate: Text("Living Brain is up to date.").foregroundStyle(.secondary)
        case .verifying: Text("Verifying the download…").foregroundStyle(.secondary)
        case .readyToInstall: Text("Preparing the update…").foregroundStyle(.secondary)
        case .installing: Text("Relaunching…").foregroundStyle(.secondary)
        case let .failed(message): Text(message).font(.callout).foregroundStyle(.red)
        case .idle, .available, .downloading: EmptyView()
        }
    }

    // MARK: Actions

    private func reload() async {
        guard workspace.signedIn else { return }
        do { models = try await workspace.call { try await $0.models() }; modelsError = nil }
        catch { modelsError = error.localizedDescription }
        do { tokens = try await workspace.call { try await $0.tokens() }; tokensError = nil }
        catch { tokensError = error.localizedDescription }
        if catalog == nil { catalog = try? await workspace.client.providerCatalog() }
    }

    private func disconnect(_ role: ModelRole) {
        Task {
            do { try await workspace.call { try await $0.disconnectModel(role: role) } }
            catch { modelsError = error.localizedDescription }
            await reload()
        }
    }

    private func revoke(_ token: TokenSummary) {
        let alert = NSAlert()
        alert.messageText = "Revoke “\(token.name)”?"
        alert.informativeText = "Anything using \(token.prefix)… stops working immediately."
        alert.addButton(withTitle: "Revoke")
        alert.addButton(withTitle: "Cancel")
        alert.buttons.first?.hasDestructiveAction = true
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        Task {
            do { try await workspace.call { try await $0.revokeToken(prefix: token.prefix) } }
            catch { tokensError = error.localizedDescription }
            await reload()
        }
    }
}


/// Connect or change one role's model, as the web Settings form does.
struct ModelEditor: View {
    let role: ModelRole
    let current: ModelConnection?
    let catalog: ProviderCatalog?
    let workspace: WorkspaceModel
    let done: () async -> Void
    @Environment(\.dismiss) private var dismiss
    @State private var provider = "anthropic"
    @State private var apiKey = ""
    @State private var model = ""
    @State private var baseURL = ""
    @State private var wire = "openai"
    @State private var auth = "bearer"
    @State private var variables: [String: String] = [:]
    @State private var discovered: [String] = []
    @State private var busy = false
    @State private var error: String?

    private var providers: [ProviderCatalog.Provider] { catalog?.all ?? [] }
    private var selected: ProviderCatalog.Provider? { providers.first { $0.id == provider } }
    private var isCustom: Bool { provider == "custom" }

    private var request: ModelConnectRequest {
        ModelConnectRequest(provider: provider, apiKey: apiKey, model: model,
                            baseUrl: isCustom ? baseURL : nil, variables: variables,
                            wire: wire, auth: auth)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("\(role.title) model").font(.title2.weight(.semibold))
            Form {
                Picker("Provider", selection: $provider) {
                    ForEach(providers) { Text($0.name).tag($0.id) }
                    Divider()
                    Text("Custom endpoint").tag("custom")
                }
                if isCustom {
                    TextField("Base URL", text: $baseURL, prompt: Text("https://…/v1"))
                    Picker("API", selection: $wire) {
                        Text("OpenAI Chat Completions").tag("openai")
                        Text("Anthropic Messages").tag("anthropic")
                    }
                    Picker("Key sent as", selection: $auth) {
                        Text("Authorization: Bearer").tag("bearer")
                        Text("x-api-key").tag("x-api-key")
                    }
                }
                ForEach(selected?.variables ?? [], id: \.name) { variable in
                    TextField(variable.label ?? variable.name,
                              text: Binding(get: { variables[variable.name] ?? "" }, set: { variables[variable.name] = $0 }),
                              prompt: Text(variable.placeholder ?? ""))
                }
                SecureField("API key", text: $apiKey)
                HStack {
                    TextField("Model", text: $model)
                    if !discovered.isEmpty {
                        Menu("Choose") { ForEach(discovered, id: \.self) { m in Button(m) { model = m } } }.fixedSize()
                    }
                    Button("Discover") { discover() }.disabled(busy || apiKey.isEmpty)
                }
            }
            .formStyle(.grouped)
            if let error { Text(error).foregroundStyle(.red).font(.callout) }
            HStack {
                if let site = selected?.site, let url = URL(string: site) {
                    Link("Get a key from \(selected?.name ?? "the provider")", destination: url).font(.callout)
                }
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(busy ? "Checking…" : "Connect") { connect() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(busy)
            }
        }
        .padding(20)
        .frame(width: 560)
        .onAppear {
            if let current {
                provider = current.provider
                model = current.model
                if current.provider == "custom" {
                    baseURL = current.baseUrl ?? ""
                    wire = current.wire ?? "openai"
                    auth = current.auth ?? "bearer"
                }
            }
        }
    }

    private func discover() {
        busy = true
        error = nil
        Task {
            do { discovered = try await workspace.call { try await $0.discoverModels(request) } }
            catch { self.error = error.localizedDescription }
            busy = false
        }
    }

    private func connect() {
        let request = self.request
        if let first = request.validationErrors.first { error = first; return }
        busy = true
        error = nil
        Task {
            do {
                _ = try await workspace.call { try await $0.connectModel(role: role, request) }
                await done()
                dismiss()
            } catch {
                self.error = error.localizedDescription
            }
            busy = false
        }
    }
}
