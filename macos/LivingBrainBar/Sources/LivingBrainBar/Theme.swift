import AppKit
import SwiftUI

/// Living Brain's brand, from crates/livingbrain-venture/src/mail-theme.json.
enum Theme {
    /// #00704e in light mode, #56f1b0 in dark.
    static let accent = Color(nsColor: NSColor(name: "LivingBrainAccent") { appearance in
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
            ? NSColor(srgbRed: 0x56 / 255, green: 0xF1 / 255, blue: 0xB0 / 255, alpha: 1)
            : NSColor(srgbRed: 0x00 / 255, green: 0x70 / 255, blue: 0x4E / 255, alpha: 1)
    })

    static let mono = Font.system(.body, design: .monospaced)
}

/// A centred empty / error / loading placeholder.
struct StatePlaceholder: View {
    enum Kind { case empty, loading, error }
    var kind: Kind
    var title: String
    var message: String? = nil
    var systemImage: String = "tray"
    var retry: (() -> Void)? = nil

    var body: some View {
        switch kind {
        case .loading:
            VStack(spacing: 10) {
                ProgressView()
                Text(title).foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        case .empty, .error:
            ContentUnavailableView {
                Label(title, systemImage: kind == .error ? "exclamationmark.triangle" : systemImage)
            } description: {
                if let message { Text(message) }
            } actions: {
                if let retry { Button("Try Again", action: retry) }
            }
        }
    }
}

extension Int {
    /// 1234567 → "1.2M".
    var compact: String {
        formatted(.number.notation(.compactName).precision(.fractionLength(0...1)))
    }
}
