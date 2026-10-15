import AppKit
import LivingBrainKit
import SwiftUI

@main
struct LivingBrainBarApp: App {
    @StateObject private var model = BarModel()

    var body: some Scene {
        MenuBarExtra {
            BarMenu(model: model)
        } label: {
            Image(nsImage: MenuIcon.image(for: model.state.phase))
                .accessibilityLabel(MenuIcon.accessibilityLabel(for: model.state.phase))
        }
        .menuBarExtraStyle(.menu)
    }
}

struct BarMenu: View {
    @ObservedObject var model: BarModel

    var body: some View {
        Button(model.state.isSyncing ? "Syncing…" : "Sync Now") { model.syncNow() }
            .keyboardShortcut("r")
            .disabled(model.state.isSyncing)

        Text(model.state.lastSyncLine())
        if let report = model.state.lastReport {
            Text(report.summary)
        }
        if let message = model.state.errorMessage {
            Text("⚠︎ " + message)
        }

        if model.signingIn {
            if let prompt = model.loginPrompt {
                Text("Enter code \(prompt.userCode) in the browser")
                if let url = prompt.openURL {
                    Button("Open Sign-in Page Again") { model.open(url) }
                }
            } else {
                Text("Starting sign-in…")
            }
            Button("Cancel Sign-in") { model.cancelSignIn() }
        } else if model.state.needsSignIn {
            Button("Sign In…") { model.signIn() }
        }

        Divider()

        Menu("Synced Projects (\(model.projects.count))") {
            if model.projects.isEmpty {
                Text("No repositories opted in")
            }
            ForEach(model.projects) { project in
                Menu(project.name) {
                    Text(project.displayPath())
                    Button("Reveal in Finder") { model.reveal(project) }
                    Button("Remove") { model.removeProject(project) }
                }
            }
            if let error = model.projectsError {
                Divider()
                Text("⚠︎ " + error)
            }
        }
        Button("Add Project…") { model.addProject() }

        Divider()

        Button("Open Living Brain") { model.open(BarModel.appURL) }
            .keyboardShortcut("o")
        Button("Agent Sessions") { model.open(BarModel.sessionsURL) }

        Divider()

        Toggle("Sync Automatically Every 15 Min", isOn: $model.autoSync)
        Toggle("Launch at Login", isOn: Binding(
            get: { model.launchAtLogin },
            set: { model.setLaunchAtLogin($0) }
        ))
        if let error = model.launchAtLoginError {
            Text("⚠︎ " + error)
        }
        Menu("CLI") {
            Text(model.cliPathLabel)
            Button("Choose CLI…") { model.chooseCLI() }
            Button("Use Default Location") { model.resetCLI() }
        }

        Divider()

        Button("Quit Living Brain") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}

/// The menu bar glyph: the brain symbol, with a small dot for state —
/// hollow while syncing, filled on error. Template-rendered, so it follows
/// the menu bar's light/dark appearance.
enum MenuIcon {
    static func image(for phase: SyncPhase) -> NSImage {
        let size = NSSize(width: 22, height: 18)
        let symbol = NSImage(systemSymbolName: "brain.head.profile", accessibilityDescription: nil)?
            .withSymbolConfiguration(.init(pointSize: 14, weight: .regular))
        let image = NSImage(size: size, flipped: false) { rect in
            if let symbol {
                let s = symbol.size
                symbol.draw(in: NSRect(x: 1, y: (rect.height - s.height) / 2, width: s.width, height: s.height))
            }
            let dot = NSRect(x: rect.width - 7, y: 1, width: 6, height: 6)
            switch phase {
            case .idle:
                break
            case .syncing:
                let path = NSBezierPath(ovalIn: dot.insetBy(dx: 0.75, dy: 0.75))
                path.lineWidth = 1.5
                NSColor.black.setStroke()
                path.stroke()
            case .error:
                NSColor.black.setFill()
                NSBezierPath(ovalIn: dot).fill()
            }
            return true
        }
        image.isTemplate = true
        return image
    }

    static func accessibilityLabel(for phase: SyncPhase) -> String {
        switch phase {
        case .idle: return "Living Brain"
        case .syncing: return "Living Brain — syncing"
        case .error: return "Living Brain — sync error"
        }
    }
}
