import LivingBrainKit
import SwiftUI

/// Renders a wiki page's Markdown natively: blocks from `Markdown.blocks`,
/// inline syntax through `AttributedString`. `[[wikilinks]]` become
/// `livingbrain://page/<slug>` links; `onPage` receives them.
struct MarkdownView: View {
    let markdown: String
    var onPage: ((String) -> Void)? = nil

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(Array(Markdown.blocks(markdown).enumerated()), id: \.offset) { _, block in
                view(for: block)
            }
        }
        .textSelection(.enabled)
        .environment(\.openURL, OpenURLAction { url in
            if url.scheme == "livingbrain", url.host == "page" {
                onPage?(url.lastPathComponent)
                return .handled
            }
            return .systemAction
        })
    }

    private func inline(_ text: String) -> AttributedString {
        let linked = Markdown.linkWikilinks(text)
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: linked, options: options)) ?? AttributedString(text)
    }

    @ViewBuilder private func view(for block: MarkdownBlock) -> some View {
        switch block {
        case let .heading(level, text):
            Text(inline(text))
                .font(level == 1 ? .title.weight(.semibold) : level == 2 ? .title2.weight(.semibold) : .title3.weight(.semibold))
                .padding(.top, level <= 2 ? 6 : 2)
        case let .paragraph(text):
            Text(inline(text)).fixedSize(horizontal: false, vertical: true)
        case let .bullets(items):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("•").foregroundStyle(Theme.accent)
                        Text(inline(item)).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        case let .numbered(items):
            VStack(alignment: .leading, spacing: 4) {
                ForEach(Array(items.enumerated()), id: \.offset) { index, item in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Text("\(index + 1).").monospacedDigit().foregroundStyle(.secondary)
                        Text(inline(item)).fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        case let .quote(text):
            HStack(spacing: 10) {
                RoundedRectangle(cornerRadius: 1.5).fill(Theme.accent.opacity(0.6)).frame(width: 3)
                Text(inline(text)).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
            }
        case let .code(_, text):
            ScrollView(.horizontal) {
                Text(text).font(.system(.callout, design: .monospaced)).padding(10)
            }
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.secondary.opacity(0.1)))
        case .rule:
            Divider()
        }
    }
}
