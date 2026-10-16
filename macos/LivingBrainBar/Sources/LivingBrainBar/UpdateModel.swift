import AppKit
import Foundation
import LivingBrainKit

/// Plain-English failures for the update flow.
enum UpdateError: LocalizedError {
    case noBundleIdentifier
    case server(Int)
    case badChecksumFile
    case checksumMismatch
    case mountFailed
    case wrongApp
    case versionMismatch
    case signature
    case copyFailed
    case helper

    var errorDescription: String? {
        switch self {
        case .noBundleIdentifier:
            return "This build of Living Brain cannot update itself. Install a release from the website instead."
        case let .server(status):
            return "GitHub returned an error (HTTP \(status)). Try again later."
        case .badChecksumFile:
            return "The release's checksum file is unreadable, so the update was not installed."
        case .checksumMismatch:
            return "The downloaded file does not match its published checksum, so nothing was installed. Try the download again."
        case .mountFailed:
            return "The update disk image could not be opened."
        case .wrongApp:
            return "The downloaded disk image does not contain this version of Living Brain, so nothing was installed."
        case .versionMismatch:
            return "The downloaded app is not the version the release names, so nothing was installed."
        case .signature:
            return "The downloaded app failed its code signature check, so nothing was installed."
        case .copyFailed:
            return "The update could not be copied next to the installed app. Check that its folder is writable."
        case .helper:
            return "The update helper could not start, so the app was not replaced."
        }
    }
}

/// The update flow: periodic and manual checks against GitHub, then a
/// verified, in-place install. Nothing here runs with elevated privileges.
@MainActor
final class UpdateModel: ObservableObject {
    enum State: Equatable {
        case idle
        case checking
        case upToDate
        case available
        case downloading(Double)
        case verifying
        case readyToInstall
        case installing
        case failed(String)
    }

    enum Keys {
        static let autoCheck = "updates.autoCheck"
        static let skipped = "updates.skippedVersion"
        static let lastChecked = "updates.lastChecked"
    }

    @Published private(set) var state: State = .idle
    /// The newest eligible release, newer than the running app or not.
    @Published private(set) var release: Release?
    @Published private(set) var lastChecked: Date?
    @Published private(set) var skippedVersion: String?
    @Published var autoCheck: Bool {
        didSet { defaults.set(autoCheck, forKey: Keys.autoCheck) }
    }

    let currentVersion: String
    private let defaults = UserDefaults.standard
    private var schedule: Task<Void, Never>?

    init() {
        currentVersion = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.0.0"
        defaults.register(defaults: [Keys.autoCheck: true])
        autoCheck = defaults.bool(forKey: Keys.autoCheck)
        skippedVersion = defaults.string(forKey: Keys.skipped)
        lastChecked = defaults.object(forKey: Keys.lastChecked) as? Date
        // First check ten seconds after launch, then every six hours. The
        // automatic checks respect the setting; the menu's check always runs.
        schedule = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(10))
            while !Task.isCancelled, let model = self {
                if model.autoCheck { await model.check(manual: false) }
                try? await Task.sleep(for: .seconds(6 * 60 * 60))
            }
        }
    }

    // MARK: Derived state

    /// A release newer than the running app, whether or not it was skipped.
    var newerRelease: Release? {
        guard let release, isNewer(release, than: currentVersion) else { return nil }
        return release
    }

    /// What the menu offers: a newer release unless it was skipped.
    var offeredRelease: Release? {
        guard let newer = newerRelease, newer.version.description != skippedVersion else { return nil }
        return newer
    }

    /// Only a real app bundle (with an id and a version) checks and installs.
    var canUpdate: Bool {
        Bundle.main.bundleIdentifier != nil
            && (Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String).flatMap(SemVer.init) != nil
    }

    /// The app can replace itself only when its folder is writable (not the
    /// mounted disk image, not a read-only location).
    var canInstallInPlace: Bool {
        FileManager.default.isWritableFile(atPath: Bundle.main.bundleURL.deletingLastPathComponent().path)
    }

    var isBusy: Bool {
        switch state {
        case .checking, .downloading, .verifying, .readyToInstall, .installing: return true
        default: return false
        }
    }

    // MARK: Checking

    func checkForUpdates() {
        Task { await check(manual: true) }
    }

    func check(manual: Bool) async {
        guard !isBusy else { return }
        // A build run from a plain binary (no bundle id or version) cannot
        // replace itself, so it does not check at all.
        guard canUpdate else {
            state = manual ? .failed(message(for: UpdateError.noBundleIdentifier)) : .idle
            return
        }
        let previous = state
        state = .checking
        do {
            let (data, response) = try await URLSession.shared.data(for: releasesRequest(currentVersion: currentVersion))
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            if isRateLimited(status: status) {
                // Quiet on the automatic check; the next interval tries again.
                state = manual ? .failed("GitHub is limiting update checks right now. Try again in a little while.") : settled(previous)
                return
            }
            guard status == 200 else { throw UpdateError.server(status) }
            let now = Date()
            lastChecked = now
            defaults.set(now, forKey: Keys.lastChecked)
            release = latestRelease(from: data)
            state = newerRelease == nil ? .upToDate : .available
        } catch {
            state = manual ? .failed(message(for: error)) : settled(previous)
        }
    }

    /// The state an automatic check falls back to when it cannot finish.
    private func settled(_ previous: State) -> State {
        switch previous {
        case .available, .upToDate: return previous
        default: return .idle
        }
    }

    func skip(_ release: Release) {
        skippedVersion = release.version.description
        defaults.set(skippedVersion, forKey: Keys.skipped)
    }

    func openReleasePage() {
        NSWorkspace.shared.open(release?.htmlURL ?? UpdateRelease.releasePageURL)
    }

    // MARK: Installing

    func installUpdate() {
        guard let release = newerRelease, !isBusy else { return }
        guard canInstallInPlace else {
            openReleasePage()
            return
        }
        Task { await install(release) }
    }

    private func install(_ release: Release) async {
        guard let bundleID = Bundle.main.bundleIdentifier else {
            state = .failed(message(for: UpdateError.noBundleIdentifier))
            return
        }
        let fm = FileManager.default
        let installed = Bundle.main.bundleURL
        let id = UUID().uuidString
        let work = fm.temporaryDirectory.appendingPathComponent("LivingBrainUpdate-\(id)", isDirectory: true)
        let helperDir = fm.temporaryDirectory.appendingPathComponent("LivingBrainUpdater-\(id)", isDirectory: true)
        let paths = UpdateHelper.paths(installed: installed, id: id)
        var mounted: URL?
        do {
            try fm.createDirectory(at: work, withIntermediateDirectories: true)
            try fm.createDirectory(at: helperDir, withIntermediateDirectories: true)

            // 1. The checksum first (it is tiny), then the disk image.
            let (sumData, sumResponse) = try await URLSession.shared.data(for: request(release.sha256URL))
            guard (sumResponse as? HTTPURLResponse)?.statusCode == 200, sumData.count < 1024,
                  let expected = parseChecksum(String(decoding: sumData, as: UTF8.self)) else {
                throw UpdateError.badChecksumFile
            }
            let dmg = work.appendingPathComponent(UpdateRelease.dmgName)
            state = .downloading(0)
            try await UpdateDownloader.download(release.dmgURL, to: dmg, request: request(release.dmgURL)) { [weak self] fraction in
                Task { @MainActor in self?.setProgress(fraction) }
            }

            // 2. Verify the bytes before anything touches them.
            state = .verifying
            let actual = try await Task.detached(priority: .utility) { try sha256(of: dmg) }.value
            guard actual == expected else { throw UpdateError.checksumMismatch }
            state = .readyToInstall

            // 3. Mount read-only, no Finder window, and check what is inside.
            let mount = work.appendingPathComponent("mount", isDirectory: true)
            try fm.createDirectory(at: mount, withIntermediateDirectories: true)
            let attach = try await run("/usr/bin/hdiutil",
                                       ["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint", mount.path, dmg.path])
            guard attach.succeeded else { throw UpdateError.mountFailed }
            mounted = mount
            let newApp = mount.appendingPathComponent("Living Brain.app", isDirectory: true)
            guard let bundle = Bundle(url: newApp), bundle.bundleIdentifier == bundleID else { throw UpdateError.wrongApp }
            guard bundle.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String == release.version.description else {
                throw UpdateError.versionMismatch
            }
            let signature = try await run("/usr/bin/codesign", ["--verify", "--deep", "--strict", newApp.path])
            guard signature.succeeded else { throw UpdateError.signature }

            // 4. Copy beside the installed app (same volume, so the swap is a rename).
            let copy = try await run("/usr/bin/ditto", [newApp.path, paths.staging.path])
            guard copy.succeeded else { throw UpdateError.copyFailed }

            // 5. Detach the image and the download; keep only the helper.
            await detach(mount)
            mounted = nil
            try? fm.removeItem(at: work)

            // 6. Hand off. The helper waits for this process to exit, then swaps.
            let script = try UpdateHelper.writeScript(in: helperDir)
            let helper = Process()
            helper.executableURL = URL(fileURLWithPath: "/bin/sh")
            helper.arguments = [
                script.path,
                String(ProcessInfo.processInfo.processIdentifier),
                paths.staging.path,
                installed.path,
                paths.backup.path,
                helperDir.path,
                "/usr/bin/open",
            ]
            helper.standardOutput = FileHandle.nullDevice
            helper.standardError = FileHandle.nullDevice
            helper.standardInput = FileHandle.nullDevice
            do {
                try helper.run()
            } catch {
                throw UpdateError.helper
            }
            state = .installing
            NSApp.terminate(nil)
        } catch {
            if let mounted { await detach(mounted) }
            try? fm.removeItem(at: work)
            try? fm.removeItem(at: helperDir)
            try? fm.removeItem(at: paths.staging)
            state = .failed(message(for: error))
        }
    }

    private func setProgress(_ fraction: Double) {
        if case .downloading = state { state = .downloading(fraction) }
    }

    private func detach(_ mount: URL) async {
        let plain = try? await run("/usr/bin/hdiutil", ["detach", mount.path, "-quiet"])
        if plain?.succeeded != true {
            _ = try? await run("/usr/bin/hdiutil", ["detach", mount.path, "-force", "-quiet"])
        }
    }

    // MARK: Helpers

    private func run(_ executable: String, _ arguments: [String]) async throws -> CLIResult {
        try await CLIRunner.run(executable: executable, arguments: arguments,
                                environment: ProcessInfo.processInfo.environment)
    }

    private func request(_ url: URL) -> URLRequest {
        var request = URLRequest(url: url)
        request.timeoutInterval = 60
        request.setValue("LivingBrain-Mac/\(currentVersion)", forHTTPHeaderField: "User-Agent")
        return request
    }

    private func message(for error: Error) -> String {
        if let known = error as? UpdateError { return known.localizedDescription }
        if error is URLError {
            return "Could not reach GitHub to check for updates. Check your internet connection and try again."
        }
        return error.localizedDescription
    }
}
