import LivingBrainKit
import SwiftUI

struct AskView: View {
    @ObservedObject var workspace: WorkspaceModel
    @ObservedObject var router: Router
    @StateObject private var projects = ProjectList()
    @State private var question = ""
    @State private var project = ""
    @State private var asked: String?
    @State private var answer: Answer?
    @State private var asking = false
    @State private var error: String?
    @FocusState private var focused: Bool

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Image(systemName: "questionmark.bubble").foregroundStyle(Theme.accent)
                TextField("Ask the brain a question", text: $question, axis: .vertical)
                    .textFieldStyle(.plain)
                    .font(.title3)
                    .lineLimit(1...4)
                    .focused($focused)
                    .onSubmit(ask)
                Button(asking ? "Asking…" : "Ask", action: ask)
                    .buttonStyle(.borderedProminent)
                    .disabled(asking || question.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(16)
            Divider()
            content.frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .navigationTitle("Ask")
        .toolbar { ToolbarItem { ProjectPicker(selection: $project, names: projects.names) } }
        .task {
            focused = true
            await projects.load(workspace)
        }
    }

    @ViewBuilder private var content: some View {
        if asking {
            StatePlaceholder(kind: .loading, title: "Reading the pages you can see…")
        } else if let error {
            StatePlaceholder(kind: .error, title: "Could not ask", message: error, retry: ask)
        } else if let answer {
            ScrollView {
                VStack(alignment: .leading, spacing: 18) {
                    if let asked { Text(asked).font(.title3.weight(.semibold)) }
                    MarkdownView(markdown: answer.answer) { router.openPage($0) }
                    if !answer.citations.isEmpty {
                        Divider()
                        Text("Sources").font(.headline)
                        ForEach(Array(answer.citations.enumerated()), id: \.offset) { index, citation in
                            CitationRow(index: index + 1, citation: citation) {
                                if let slug = citation.slug { router.openPage(slug) }
                            }
                        }
                    }
                }
                .padding(24)
                .frame(maxWidth: 820, alignment: .leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        } else {
            StatePlaceholder(kind: .empty, title: "Ask anything your wiki knows",
                             message: project.isEmpty ? "Answers cite the pages they came from." : "Scoped to \(project).",
                             systemImage: "sparkle.magnifyingglass")
        }
    }

    private func ask() {
        let q = question.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty, !asking else { return }
        asking = true
        error = nil
        let scope = project
        Task {
            do {
                answer = try await workspace.call { try await $0.ask(q, project: scope) }
                asked = q
            } catch {
                self.error = error.localizedDescription
            }
            asking = false
        }
    }
}

struct CitationRow: View {
    let index: Int
    let citation: Citation
    let open: () -> Void

    var body: some View {
        Button(action: open) {
            HStack(alignment: .top, spacing: 10) {
                Text("\(index)")
                    .font(.caption.weight(.bold)).monospacedDigit()
                    .frame(width: 20, height: 20)
                    .background(Circle().fill(Theme.accent.opacity(0.18)))
                VStack(alignment: .leading, spacing: 3) {
                    Text(citation.title).fontWeight(.medium)
                    if let quote = citation.quote, !quote.isEmpty {
                        Text(quote).font(.callout).foregroundStyle(.secondary).lineLimit(3)
                    }
                }
                Spacer()
                Image(systemName: "chevron.right").foregroundStyle(.tertiary)
            }
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 10).fill(Color.secondary.opacity(0.07)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(citation.slug == nil)
    }
}
