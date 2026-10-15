import AppKit

/// `--snapshot <file.png>`: after the window settles, write a PNG of it and
/// keep running. For README screenshots and UI checks on machines where the
/// screen-capture permission is not granted (an app may always draw its own
/// windows). `--snapshot-delay <seconds>` overrides the 8 s wait.
enum Snapshot {
    static var request: (path: String, delay: Double)? {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--snapshot"), i + 1 < args.count else { return nil }
        var delay = 8.0
        if let j = args.firstIndex(of: "--snapshot-delay"), j + 1 < args.count, let d = Double(args[j + 1]) { delay = d }
        return (args[i + 1], delay)
    }

    @MainActor
    static func takeIfRequested() async {
        guard let request else { return }
        try? await Task.sleep(nanoseconds: UInt64(request.delay * 1_000_000_000))
        guard let window = NSApp.windows.first(where: { $0.isVisible && $0.canBecomeMain }),
              let view = window.contentView?.superview ?? window.contentView,
              let rep = view.bitmapImageRepForCachingDisplay(in: view.bounds)
        else { return }
        view.cacheDisplay(in: view.bounds, to: rep)
        if let data = rep.representation(using: .png, properties: [:]) {
            try? data.write(to: URL(fileURLWithPath: request.path))
        }
    }
}
