import AppKit
import LivingBrainKit
import SwiftUI

/// Owns the models so both scenes share one instance of each.
@MainActor
final class AppState: ObservableObject {
    let bar = BarModel()
    lazy var sessions = SessionsModel(bar: bar)
    lazy var workspace = WorkspaceModel(bar: bar)
    let router = Router()
}

@main
struct LivingBrainBarApp: App {
    @StateObject private var app = AppState()

    var body: some Scene {
        MenuBarExtra {
            BarMenu(model: app.bar)
        } label: {
            MenuBarLabel(model: app.bar)
        }
        .menuBarExtraStyle(.menu)

        Window("Living Brain", id: "main") {
            MainWindow(bar: app.bar, sessions: app.sessions, workspace: app.workspace, router: app.router)
        }
        .defaultSize(width: 1180, height: 740)
        .commands {
            CommandGroup(replacing: .newItem) {
                Button("New Note") {
                    app.router.destination = .wiki
                    app.router.newNoteRequest += 1
                }
                .keyboardShortcut("n")
            }
            CommandGroup(after: .newItem) {
                Button("Search") {
                    app.router.destination = .wiki
                    app.router.searchRequest += 1
                }
                .keyboardShortcut("k")
                Button("Sync Now") { app.bar.syncNow() }
                    .keyboardShortcut("r")
            }
            CommandMenu("Go") {
                ForEach(Array((Destination.workspace + Destination.local + [.settings]).enumerated()), id: \.offset) { index, destination in
                    Button(destination.title) { app.router.destination = destination }
                        .keyboardShortcut(KeyEquivalent(Character(String(index + 1))), modifiers: .command)
                }
            }
        }
    }
}

struct MenuBarLabel: View {
    @ObservedObject var model: BarModel
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Image(nsImage: MenuIcon.image(for: model.state.phase))
            .accessibilityLabel(MenuIcon.accessibilityLabel(for: model.state.phase))
            .task {
                // `open "Living Brain.app" --args --show-window` starts with
                // the window open (the default is menu bar only).
                if CommandLine.arguments.contains("--show-window") {
                    openWindow(id: "main")
                }
            }
    }
}

struct BarMenu: View {
    @ObservedObject var model: BarModel
    @Environment(\.openWindow) private var openWindow

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
        if model.paused {
            Text("Sync is paused")
        } else if model.pendingFiles > 0 {
            Text("Watching \(model.pendingFiles) active session\(model.pendingFiles == 1 ? "" : "s")")
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
                    Button("Reveal in Finder") { model.reveal(path: project.path) }
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

        Button("Open Living Brain Window…") {
            openWindow(id: "main")
            NSApp.activate(ignoringOtherApps: true)
        }
        .keyboardShortcut("0")
        Button("Open Living Brain") { model.open(model.appURL) }
            .keyboardShortcut("o")
        Button("Agent Sessions") { model.open(model.sessionsURL) }

        Divider()

        Toggle("Pause Syncing", isOn: $model.paused)
        Toggle("Live Sync (When a Session Goes Quiet)", isOn: $model.liveSync)
        Toggle("Sync Automatically Every 15 Min", isOn: $model.autoSync)
        Toggle("Notifications", isOn: $model.notifications)
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
