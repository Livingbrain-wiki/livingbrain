import AppKit
import LivingBrainKit
import SwiftUI

/// Loads `/v1/sources/projects` once for the project pickers.
@MainActor
final class ProjectList: ObservableObject {
    @Published private(set) var names: [String] = []
    private var loaded = false

    func load(_ workspace: WorkspaceModel) async {
        guard !loaded else { return }
        if let projects = try? await workspace.call({ try await $0.sourceProjects() }) {
            names = projects.map(\.project).sorted { $0.localizedCaseInsensitiveCompare($1) == .orderedAscending }
            loaded = true
        }
    }
}

struct ProjectPicker: View {
    @Binding var selection: String
    let names: [String]

    var body: some View {
        Picker(selection: $selection) {
            Text("All Projects").tag("")
            if !names.isEmpty { Divider() }
            ForEach(names, id: \.self) { Text($0).tag($0) }
        } label: {
            Label("Project", systemImage: "folder")
        }
        .frame(minWidth: 150)
        .help("Scope to one project")
    }
}

@MainActor
final class WikiModel: ObservableObject {
    enum Load<T: Equatable>: Equatable {
        case idle, loading, loaded(T), failed(String)
    }

    @Published var query = ""
    @Published var project = ""
    @Published private(set) var pages: Load<[PageSummary]> = .idle
    @Published private(set) var results: Load<[SearchResult]> = .idle
    @Published var selectedSlug: String?
    @Published private(set) var page: Load<Page> = .idle
    @Published var editing = false
    @Published var draft = ""
    @Published private(set) var saving = false
    @Published var conflict: String?
    @Published var saveError: String?
    @Published var notice: String?

    let workspace: WorkspaceModel
    private var searchTask: Task<Void, Never>?

    init(workspace: WorkspaceModel) {
        self.workspace = workspace
    }

    func loadPages() {
        pages = .loading
        Task {
            do { pages = .loaded(try await workspace.call { try await $0.pages() }) }
            catch { pages = .failed(error.localizedDescription) }
        }
    }

    func search() {
        searchTask?.cancel()
        let q = query.trimmingCharacters(in: .whitespaces)
        guard !q.isEmpty else { results = .idle; return }
        results = .loading
        let project = self.project
        searchTask = Task {
            try? await Task.sleep(nanoseconds: 250_000_000) // debounce typing
            guard !Task.isCancelled else { return }
            do {
                let found = try await workspace.call { try await $0.search(q, project: project, limit: 25) }
                if !Task.isCancelled { results = .loaded(found) }
            } catch {
                if !Task.isCancelled { results = .failed(error.localizedDescription) }
            }
        }
    }

    func open(_ slug: String) {
        if editing, selectedSlug != slug { editing = false }
        selectedSlug = slug
        page = .loading
        Task {
            do {
                let loaded = try await workspace.call { try await $0.page(slug) }
                if selectedSlug == slug { page = .loaded(loaded) }
            } catch {
                if selectedSlug == slug { page = .failed(error.localizedDescription) }
            }
        }
    }

    var currentPage: Page? {
        if case let .loaded(p) = page { return p }
        return nil
    }

    func beginEdit() {
        guard let p = currentPage else { return }
        draft = p.markdown
        editing = true
    }

    /// Save with the version that was read; a 409 offers to reload theirs.
    func save(overwrite: Bool = false) {
        guard let p = currentPage else { return }
        saving = true
        saveError = nil
        Task {
            defer { saving = false }
            do {
                var base = p.version
                if overwrite {
                    base = try await workspace.call { try await $0.page(p.slug) }.version
                }
                let saved = try await workspace.call { try await $0.savePage(p.slug, markdown: draft, baseVersion: base) }
                page = .loaded(saved)
                editing = false
                conflict = nil
                // The server redacts on arrival: say so when it changed the text.
                notice = saved.markdown == draft ? "Saved version \(saved.version ?? 0)." :
                    "Saved version \(saved.version ?? 0). The server redacted part of the text."
                if case .loaded = pages { loadPages() }
            } catch let APIError.conflict(message) {
                conflict = message
            } catch {
                saveError = error.localizedDescription
            }
        }
    }

    /// Drop the edit and show the current version.
    func loadTheirs() {
        conflict = nil
        editing = false
        if let slug = selectedSlug { open(slug) }
    }

    func createNote(title: String, project: String, body: String) async -> String? {
        do {
            let note = try await workspace.call { try await $0.createNote(body: body, title: title, project: project) }
            notice = note.unchanged == true ? "That note already exists." : "Note saved."
            loadPages()
            open(note.slug)
            return nil
        } catch {
            return error.localizedDescription
        }
    }

    func webURL(for slug: String) -> URL {
        var c = URLComponents(url: workspace.baseURL.appendingPathComponent("wiki.html"), resolvingAgainstBaseURL: false)!
        c.queryItems = [URLQueryItem(name: "slug", value: slug)]
        return c.url!
    }
}

struct WikiView: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var router: Router
    @StateObject private var model: WikiModel
    @StateObject private var projects = ProjectList()
    @FocusState private var searchFocused: Bool
    @State private var showingNewNote = false

    init(workspace: WorkspaceModel, router: Router) {
        self.workspace = workspace
        self.router = router
        _model = StateObject(wrappedValue: WikiModel(workspace: workspace))
    }

    var body: some View {
        HSplitView {
            sidebar.frame(minWidth: 260, idealWidth: 300, maxWidth: 420, maxHeight: .infinity)
            pageDetail.frame(minWidth: 420, maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle("Wiki")
        .toolbar {
            ToolbarItemGroup {
                ProjectPicker(selection: $model.project, names: projects.names)
                Button { showingNewNote = true } label: { Label("New Note", systemImage: "square.and.pencil") }
                    .help("New note (⌘N)")
            }
        }
        .sheet(isPresented: $showingNewNote) {
            NewNoteSheet(projects: projects.names, initialProject: model.project) { title, project, body in
                await model.createNote(title: title, project: project, body: body)
            }
        }
        .task {
            if case .idle = model.pages { model.loadPages() }
            await projects.load(workspace)
            consumeRouter()
        }
        .onChange(of: router.pendingSlug) { consumeRouter() }
        .onChange(of: router.searchRequest) { searchFocused = true }
        .onChange(of: router.newNoteRequest) { showingNewNote = true }
        .onChange(of: model.query) { model.search() }
        .onChange(of: model.project) { model.search() }
    }

    private func consumeRouter() {
        if let slug = router.pendingSlug {
            router.pendingSlug = nil
            model.open(slug)
        }
    }

    // MARK: Left column

    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Search the brain  ⌘K", text: $model.query)
                    .textFieldStyle(.plain)
                    .focused($searchFocused)
                if !model.query.isEmpty {
                    Button { model.query = "" } label: { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain).foregroundStyle(.secondary)
                }
            }
            .padding(8)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.secondary.opacity(0.1)))
            .padding(10)
            Divider()
            if model.query.trimmingCharacters(in: .whitespaces).isEmpty {
                pageList
            } else {
                resultList
            }
        }
    }

    @ViewBuilder private var pageList: some View {
        switch model.pages {
        case .idle, .loading:
            StatePlaceholder(kind: .loading, title: "Loading pages…")
        case let .failed(message):
            StatePlaceholder(kind: .error, title: "Could not load pages", message: message, retry: model.loadPages)
        case let .loaded(pages) where pages.isEmpty:
            StatePlaceholder(kind: .empty, title: "No pages yet", message: "Write a note with ⌘N.", systemImage: "book.pages")
        case let .loaded(pages):
            List(pages, selection: Binding(get: { model.selectedSlug }, set: { if let s = $0 { model.open(s) } })) { page in
                VStack(alignment: .leading, spacing: 2) {
                    Text(page.slug).lineLimit(1)
                    HStack(spacing: 6) {
                        Text(page.entityType ?? "page")
                        if let date = page.updatedDate { Text("· \(RelativeTime.string(date))") }
                    }
                    .font(.caption).foregroundStyle(.secondary)
                }
                .tag(page.slug)
            }
            .listStyle(.sidebar)
        }
    }

    @ViewBuilder private var resultList: some View {
        switch model.results {
        case .idle, .loading:
            StatePlaceholder(kind: .loading, title: "Searching…")
        case let .failed(message):
            StatePlaceholder(kind: .error, title: "Search failed", message: message, retry: model.search)
        case let .loaded(results) where results.isEmpty:
            StatePlaceholder(kind: .empty, title: "No matches", message: model.project.isEmpty ? nil : "Only in \(model.project).",
                             systemImage: "magnifyingglass")
        case let .loaded(results):
            List(results, selection: Binding(get: { model.selectedSlug }, set: { if let s = $0 { model.open(s) } })) { result in
                VStack(alignment: .leading, spacing: 3) {
                    Text(result.title).fontWeight(.medium).lineLimit(1)
                    if let snippet = result.snippet {
                        Text(snippet).font(.caption).foregroundStyle(.secondary).lineLimit(3)
                    }
                }
                .tag(result.slug ?? result.url)
            }
            .listStyle(.sidebar)
        }
    }

    // MARK: Right column

    @ViewBuilder private var pageDetail: some View {
        switch model.page {
        case .idle:
            StatePlaceholder(kind: .empty, title: "Pick a page", message: "Or search with ⌘K.", systemImage: "doc.text")
        case .loading:
            StatePlaceholder(kind: .loading, title: "Opening…")
        case let .failed(message):
            StatePlaceholder(kind: .error, title: "Could not open the page", message: message) {
                if let slug = model.selectedSlug { model.open(slug) }
            }
        case let .loaded(page):
            VStack(spacing: 0) {
                pageHeader(page).padding(.horizontal, 20).padding(.vertical, 12)
                Divider()
                if model.editing {
                    TextEditor(text: $model.draft)
                        .font(.system(.body, design: .monospaced))
                        .scrollContentBackground(.hidden)
                        .padding(12)
                } else {
                    ScrollView {
                        VStack(alignment: .leading, spacing: 18) {
                            MarkdownView(markdown: page.markdown) { model.open($0) }
                            if !page.backlinks.isEmpty {
                                linkSection("Linked from", page.backlinks)
                            }
                        }
                        .padding(24)
                        .frame(maxWidth: 820, alignment: .leading)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .alert("The page moved", isPresented: Binding(get: { model.conflict != nil }, set: { if !$0 { model.conflict = nil } })) {
                Button("Load Their Version") { model.loadTheirs() }
                Button("Overwrite with Mine", role: .destructive) { model.save(overwrite: true) }
                Button("Keep Editing", role: .cancel) { model.conflict = nil }
            } message: {
                Text(model.conflict ?? "")
            }
        }
    }

    private func pageHeader(_ page: Page) -> some View {
        HStack(alignment: .center, spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text(page.displayTitle).font(.title2.weight(.semibold)).lineLimit(1)
                HStack(spacing: 6) {
                    Text(page.entityType ?? "page")
                        .padding(.horizontal, 6).padding(.vertical, 1)
                        .background(Capsule().fill(Theme.accent.opacity(0.15)))
                    if let v = page.version { Text("version \(v)") }
                    if let notice = model.notice { Text("· \(notice)").foregroundStyle(Theme.accent) }
                    if let error = model.saveError { Text("· \(error)").foregroundStyle(.red).lineLimit(1) }
                }
                .font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if model.editing {
                Button("Cancel") { model.editing = false }.keyboardShortcut(.cancelAction)
                Button(model.saving ? "Saving…" : "Save") { model.save() }
                    .buttonStyle(.borderedProminent)
                    .keyboardShortcut("s")
                    .disabled(model.saving)
            } else {
                Button { model.beginEdit() } label: { Label("Edit", systemImage: "pencil") }
                    .keyboardShortcut("e")
                Button { NSWorkspace.shared.open(model.webURL(for: page.slug)) } label: {
                    Label("Open in Browser", systemImage: "safari")
                }
                .labelStyle(.iconOnly)
                .help("Open in Living Brain on the web")
            }
        }
    }

    private func linkSection(_ title: String, _ slugs: [String]) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.headline)
            FlowLinks(slugs: slugs) { model.open($0) }
        }
        .padding(.top, 8)
    }
}

struct FlowLinks: View {
    let slugs: [String]
    let open: (String) -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            ForEach(slugs, id: \.self) { slug in
                Button { open(slug) } label: { Label(slug, systemImage: "link") }
                    .buttonStyle(.link)
            }
        }
    }
}

struct NewNoteSheet: View {
    let projects: [String]
    @State var project: String
    let save: (String, String, String) async -> String?
    @State private var title = ""
    @State private var text = ""
    @State private var saving = false
    @State private var error: String?
    @Environment(\.dismiss) private var dismiss

    init(projects: [String], initialProject: String, save: @escaping (String, String, String) async -> String?) {
        self.projects = projects
        _project = State(initialValue: initialProject)
        self.save = save
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Note").font(.title2.weight(.semibold))
            TextField("Title", text: $title)
            HStack {
                Text("Project")
                TextField("optional", text: $project)
                if !projects.isEmpty {
                    Menu("Pick") {
                        ForEach(projects, id: \.self) { name in Button(name) { project = name } }
                    }
                    .fixedSize()
                }
            }
            TextEditor(text: $text)
                .font(.body)
                .frame(minHeight: 220)
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.3)))
            Text("Redacted on arrival like everything else. Markdown and [[wikilinks]] work.")
                .font(.caption).foregroundStyle(.secondary)
            if let error { Text(error).foregroundStyle(.red).font(.callout) }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button(saving ? "Saving…" : "Save Note") {
                    saving = true
                    Task {
                        error = await save(title, project, text)
                        saving = false
                        if error == nil { dismiss() }
                    }
                }
                .buttonStyle(.borderedProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(saving || text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
            }
        }
        .padding(20)
        .frame(width: 560)
    }
}
