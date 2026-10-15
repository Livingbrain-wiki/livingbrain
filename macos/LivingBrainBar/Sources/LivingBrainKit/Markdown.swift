import Foundation

/// A small block-level Markdown reader for rendering wiki pages natively:
/// headings, paragraphs, lists, quotes, fenced code and rules. Inline
/// syntax (emphasis, code, links) is left to `AttributedString(markdown:)`.
/// YAML frontmatter is split off, not rendered.
public enum MarkdownBlock: Equatable, Sendable {
    case heading(level: Int, text: String)
    case paragraph(String)
    case bullets([String])
    case numbered([String])
    case quote(String)
    case code(language: String?, text: String)
    case rule
}

public enum Markdown {
    /// The frontmatter (without fences) and the body after it.
    public static func splitFrontmatter(_ text: String) -> (frontmatter: String?, body: String) {
        let normalized = text.replacingOccurrences(of: "\r\n", with: "\n")
        guard normalized.hasPrefix("---\n") else { return (nil, normalized) }
        let rest = normalized.dropFirst(4)
        guard let end = rest.range(of: "\n---\n") ?? (rest.hasSuffix("\n---") ? rest.range(of: "\n---", options: .backwards) : nil)
        else { return (nil, normalized) }
        let front = String(rest[rest.startIndex..<end.lowerBound])
        let body = String(rest[end.upperBound...]).trimmingCharacters(in: .newlines)
        return (front, body)
    }

    /// `[[slug]]` and `[[slug|label]]` wikilinks become Markdown links to a
    /// `livingbrain:` URL the page view intercepts.
    public static func linkWikilinks(_ text: String) -> String {
        let regex = try! NSRegularExpression(pattern: #"\[\[([a-z0-9-]{1,128})(?:\|([^\]]+))?\]\]"#)
        let range = NSRange(text.startIndex..., in: text)
        var out = text
        for match in regex.matches(in: text, range: range).reversed() {
            guard let whole = Range(match.range, in: out), let slugRange = Range(match.range(at: 1), in: text) else { continue }
            let slug = String(text[slugRange])
            let label = Range(match.range(at: 2), in: text).map { String(text[$0]) } ?? slug
            out.replaceSubrange(whole, with: "[\(label)](livingbrain://page/\(slug))")
        }
        return out
    }

    public static func blocks(_ markdown: String) -> [MarkdownBlock] {
        let lines = splitFrontmatter(markdown).body.components(separatedBy: "\n")
        var blocks: [MarkdownBlock] = []
        var paragraph: [String] = []
        var index = 0

        func flush() {
            if !paragraph.isEmpty {
                blocks.append(.paragraph(paragraph.joined(separator: "\n")))
                paragraph = []
            }
        }

        func listItem(_ line: String) -> (ordered: Bool, text: String)? {
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            for marker in ["- ", "* ", "+ "] where trimmed.hasPrefix(marker) {
                return (false, String(trimmed.dropFirst(2)))
            }
            if let dot = trimmed.firstIndex(where: { $0 == "." || $0 == ")" }),
               trimmed[trimmed.startIndex..<dot].allSatisfy(\.isNumber), dot != trimmed.startIndex,
               trimmed[trimmed.index(after: dot)...].hasPrefix(" ")
            {
                return (true, String(trimmed[trimmed.index(dot, offsetBy: 2)...]))
            }
            return nil
        }

        while index < lines.count {
            let line = lines[index]
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.hasPrefix("```") || trimmed.hasPrefix("~~~") {
                flush()
                let fence = String(trimmed.prefix(3))
                let language = String(trimmed.dropFirst(3)).trimmingCharacters(in: .whitespaces)
                var code: [String] = []
                index += 1
                while index < lines.count, !lines[index].trimmingCharacters(in: .whitespaces).hasPrefix(fence) {
                    code.append(lines[index])
                    index += 1
                }
                blocks.append(.code(language: language.isEmpty ? nil : language, text: code.joined(separator: "\n")))
                index += 1
                continue
            }
            if trimmed.isEmpty {
                flush()
                index += 1
                continue
            }
            if trimmed.hasPrefix("#") {
                let level = trimmed.prefix(while: { $0 == "#" }).count
                if level <= 6, trimmed.dropFirst(level).hasPrefix(" ") {
                    flush()
                    blocks.append(.heading(level: level, text: String(trimmed.dropFirst(level + 1))))
                    index += 1
                    continue
                }
            }
            if ["---", "***", "___"].contains(trimmed) {
                flush()
                blocks.append(.rule)
                index += 1
                continue
            }
            if trimmed.hasPrefix(">") {
                flush()
                var quote: [String] = []
                while index < lines.count {
                    let t = lines[index].trimmingCharacters(in: .whitespaces)
                    guard t.hasPrefix(">") else { break }
                    quote.append(String(t.dropFirst()).trimmingCharacters(in: .whitespaces))
                    index += 1
                }
                blocks.append(.quote(quote.joined(separator: "\n")))
                continue
            }
            if let first = listItem(line) {
                flush()
                var items = [first.text]
                index += 1
                while index < lines.count {
                    let next = lines[index]
                    if let item = listItem(next), item.ordered == first.ordered {
                        items.append(item.text)
                    } else if next.hasPrefix("  "), !next.trimmingCharacters(in: .whitespaces).isEmpty {
                        items[items.count - 1] += " " + next.trimmingCharacters(in: .whitespaces)
                    } else {
                        break
                    }
                    index += 1
                }
                blocks.append(first.ordered ? .numbered(items) : .bullets(items))
                continue
            }
            paragraph.append(line)
            index += 1
        }
        flush()
        return blocks
    }
}
