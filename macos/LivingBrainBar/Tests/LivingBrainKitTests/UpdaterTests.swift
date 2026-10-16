import CryptoKit
import Foundation
import XCTest
@testable import LivingBrainKit

final class UpdaterTests: XCTestCase {
    // MARK: Release list

    private func asset(_ name: String) -> String {
        #"{"name":"\#(name)","browser_download_url":"https://github.com/Livingbrain-wiki/livingbrain/releases/download/x/\#(name)"}"#
    }

    private func release(tag: String, draft: Bool = false, prerelease: Bool = false, assets: [String] = ["LivingBrain.dmg", "LivingBrain.dmg.sha256"]) -> String {
        let list = assets.map(asset).joined(separator: ",")
        return #"{"tag_name":"\#(tag)","body":"Notes for \#(tag)","draft":\#(draft),"prerelease":\#(prerelease),"published_at":"2026-10-01T12:00:00Z","html_url":"https://github.com/Livingbrain-wiki/livingbrain/releases/tag/\#(tag)","assets":[\#(list)]}"#
    }

    private func releases(_ entries: [String]) -> Data {
        Data("[\(entries.joined(separator: ","))]".utf8)
    }

    func testPicksHighestMacosReleaseAndIgnoresTheRest() throws {
        let json = releases([
            release(tag: "macos-v0.1.2"),
            release(tag: "macos-v0.1.10"),
            release(tag: "macos-v0.1.9"),
            release(tag: "macos-v0.2.0", draft: true),
            release(tag: "macos-v0.3.0", prerelease: true),
            release(tag: "macos-v0.4.0", assets: ["LivingBrain.dmg"]),
            release(tag: "macos-v0.5.0-rc.1"),
            release(tag: "v9.9.9"),
            release(tag: "macos-v0.6.0-beta", assets: []),
        ])
        let found = try XCTUnwrap(latestRelease(from: json))
        XCTAssertEqual(found.version, SemVer(major: 0, minor: 1, patch: 10))
        XCTAssertEqual(found.dmgURL.absoluteString,
                       "https://github.com/Livingbrain-wiki/livingbrain/releases/download/x/LivingBrain.dmg")
        XCTAssertEqual(found.sha256URL.lastPathComponent, "LivingBrain.dmg.sha256")
        XCTAssertEqual(found.notes, "Notes for macos-v0.1.10")
        XCTAssertEqual(found.htmlURL.lastPathComponent, "macos-v0.1.10")
        XCTAssertNotNil(found.publishedAt)
    }

    func testMalformedEntryDoesNotHideTheOthers() throws {
        let json = releases([#"{"unexpected":true}"#, release(tag: "macos-v0.1.3")])
        XCTAssertEqual(try XCTUnwrap(latestRelease(from: json)).version.description, "0.1.3")
    }

    func testNoEligibleReleaseOrGarbageIsNil() {
        XCTAssertNil(latestRelease(from: releases([release(tag: "macos-v0.1.3", draft: true)])))
        XCTAssertNil(latestRelease(from: releases([release(tag: "v1.0.0")])))
        XCTAssertNil(latestRelease(from: Data("<html>rate limited</html>".utf8)))
        XCTAssertNil(latestRelease(from: Data("[]".utf8)))
    }

    func testNonHTTPSAssetsAreRefused() {
        let insecure = #"{"tag_name":"macos-v0.1.3","html_url":"https://github.com/x","draft":false,"prerelease":false,"assets":[{"name":"LivingBrain.dmg","browser_download_url":"http://example.com/a.dmg"},{"name":"LivingBrain.dmg.sha256","browser_download_url":"https://example.com/a.sha256"}]}"#
        XCTAssertNil(latestRelease(from: releases([insecure])))
    }

    // MARK: Versions

    func testSemVerParsing() {
        XCTAssertEqual(SemVer("0.1.2"), SemVer(major: 0, minor: 1, patch: 2))
        XCTAssertEqual(SemVer("1.0.0-rc.1"), SemVer(major: 1, minor: 0, patch: 0, prerelease: "rc.1"))
        XCTAssertEqual(SemVer("1.0.0+build.7"), SemVer(major: 1, minor: 0, patch: 0))
        for bad in ["", "1.2", "1.2.3.4", "a.b.c", "1.2.x", "-1.2.3", "1.2.3-", "v1.2.3", "1..3"] {
            XCTAssertNil(SemVer(bad), "\(bad) should not parse")
        }
    }

    func testSemVerOrdering() throws {
        XCTAssertGreaterThan(try XCTUnwrap(SemVer("0.1.10")), try XCTUnwrap(SemVer("0.1.9")))
        XCTAssertGreaterThan(try XCTUnwrap(SemVer("1.0.0")), try XCTUnwrap(SemVer("0.9.9")))
        XCTAssertFalse(try XCTUnwrap(SemVer("0.1.2")) < (try XCTUnwrap(SemVer("0.1.2"))))
        XCTAssertLessThan(try XCTUnwrap(SemVer("1.0.0-rc.1")), try XCTUnwrap(SemVer("1.0.0")))
        XCTAssertLessThan(try XCTUnwrap(SemVer("1.0.0-rc.2")), try XCTUnwrap(SemVer("1.0.0-rc.10")))
    }

    private func found(_ version: String) throws -> Release {
        Release(version: try XCTUnwrap(SemVer(version)), dmgURL: URL(string: "https://x.test/a.dmg")!,
                sha256URL: URL(string: "https://x.test/a.sha256")!, notes: "", publishedAt: nil,
                htmlURL: URL(string: "https://x.test/r")!)
    }

    func testIsNewer() throws {
        XCTAssertTrue(isNewer(try found("0.1.3"), than: "0.1.2"))
        XCTAssertTrue(isNewer(try found("0.1.10"), than: "0.1.9"))
        XCTAssertFalse(isNewer(try found("0.1.3"), than: "0.1.3"), "equal is not newer")
        XCTAssertFalse(isNewer(try found("0.1.2"), than: "0.1.3"))
        XCTAssertFalse(isNewer(try found("0.1.3"), than: "unknown"), "an unreadable current version offers nothing")
        // A pre-release is offered only to a build on the same pre-release line.
        XCTAssertFalse(isNewer(try found("0.2.0-rc.1"), than: "0.1.2"))
        XCTAssertTrue(isNewer(try found("0.2.0-rc.2"), than: "0.2.0-rc.1"))
        XCTAssertTrue(isNewer(try found("0.2.0"), than: "0.2.0-rc.1"))
    }

    func testSkippedVersionIsNotOfferedButTheNextIs() throws {
        XCTAssertFalse(shouldOffer(try found("0.1.3"), current: "0.1.2", skipped: "0.1.3"))
        XCTAssertTrue(shouldOffer(try found("0.1.4"), current: "0.1.2", skipped: "0.1.3"))
        XCTAssertTrue(shouldOffer(try found("0.1.3"), current: "0.1.2", skipped: nil))
        XCTAssertFalse(shouldOffer(try found("0.1.3"), current: "0.1.3", skipped: nil))
    }

    func testRateLimitStatuses() {
        XCTAssertTrue(isRateLimited(status: 403))
        XCTAssertTrue(isRateLimited(status: 429))
        XCTAssertFalse(isRateLimited(status: 200))
        XCTAssertFalse(isRateLimited(status: 500))
    }

    func testReleasesRequestNamesTheBuild() {
        let request = releasesRequest(currentVersion: "0.1.2")
        XCTAssertEqual(request.url?.host, "api.github.com")
        XCTAssertEqual(request.url?.path, "/repos/Livingbrain-wiki/livingbrain/releases")
        XCTAssertEqual(request.value(forHTTPHeaderField: "User-Agent"), "LivingBrain-Mac/0.1.2")
        XCTAssertEqual(request.value(forHTTPHeaderField: "Accept"), "application/vnd.github+json")
    }

    func testReleaseNotesPreview() {
        let long = (1...12).map { "line \($0)" }.joined(separator: "\n")
        XCTAssertEqual(releaseNotesPreview(long).components(separatedBy: "\n").count, 10)
        XCTAssertEqual(releaseNotesPreview("one\ntwo\n\n\n"), "one\ntwo")
        XCTAssertEqual(releaseNotesPreview(""), "")
    }

    // MARK: Checksums

    func testParseChecksum() {
        let hex = String(repeating: "ab", count: 32)
        XCTAssertEqual(parseChecksum("\(hex)  LivingBrain.dmg\n"), hex)
        XCTAssertEqual(parseChecksum("\(hex.uppercased())  LivingBrain.dmg"), hex, "uppercase is normalised")
        XCTAssertNil(parseChecksum(""))
        XCTAssertNil(parseChecksum("   \n"))
        XCTAssertNil(parseChecksum("<html>Not Found</html>"))
        XCTAssertNil(parseChecksum(String(repeating: "a", count: 63)))
        XCTAssertNil(parseChecksum(String(repeating: "g", count: 64)))
        XCTAssertNil(parseChecksum(String(repeating: "a", count: 65)))
    }

    func testSHA256OfFile() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let abc = dir.appendingPathComponent("abc.bin")
        try Data("abc".utf8).write(to: abc)
        XCTAssertEqual(try sha256(of: abc), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        let empty = dir.appendingPathComponent("empty.bin")
        try Data().write(to: empty)
        XCTAssertEqual(try sha256(of: empty), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        // Larger than one read chunk, compared with CryptoKit on the whole buffer.
        let big = Data((0..<(3 << 20)).map { UInt8($0 % 251) })
        let bigURL = dir.appendingPathComponent("big.bin")
        try big.write(to: bigURL)
        let expected = SHA256.hash(data: big).map { String(format: "%02x", $0) }.joined()
        XCTAssertEqual(try sha256(of: bigURL), expected)
    }

    // MARK: Helper

    func testHelperPathsSitBesideTheInstalledApp() {
        let installed = URL(fileURLWithPath: "/Applications/Living Brain.app")
        let paths = UpdateHelper.paths(installed: installed, id: "ID1")
        XCTAssertEqual(paths.staging.path, "/Applications/.Living Brain.update-ID1.app")
        XCTAssertEqual(paths.backup.path, "/Applications/.Living Brain.previous-ID1.app")
    }

    func testHelperReadsEveryPathFromItsArguments() {
        let script = UpdateHelper.script
        XCTAssertTrue(script.hasPrefix("#!/bin/sh"))
        for (index, name) in UpdateHelper.argumentOrder.enumerated() {
            XCTAssertTrue(script.contains("\(name)=\"$\(index + 1)\""), "\(name) must come from $\(index + 1)")
        }
        // No path is written into the text itself.
        XCTAssertFalse(script.contains("/Applications"))
        XCTAssertFalse(script.contains("Living Brain.app"))
        XCTAssertFalse(script.contains("sudo"))
        XCTAssertFalse(script.contains("osascript"))
    }

    func testHelperScriptIsValidShell() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let url = try UpdateHelper.writeScript(in: dir)
        XCTAssertEqual(try run("/bin/sh", ["-n", url.path]), 0)
        let mode = try FileManager.default.attributesOfItem(atPath: url.path)[.posixPermissions] as? Int
        XCTAssertEqual(mode, 0o700)
    }

    /// Runs the real helper on a bundle whose path has spaces, quotes and an
    /// apostrophe, with a fake opener that records its argument.
    func testHelperSwapsBundleAndRelaunchesWithAwkwardPaths() throws {
        let root = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: root) }
        let folder = root.appendingPathComponent("it's \"quoted\" dir", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let installed = try makeBundle(in: folder, named: "Living Brain.app", marker: "old")
        let newBundle = try makeBundle(in: folder, named: ".Living Brain.update-x.app", marker: "new")
        _ = try run("/usr/bin/xattr", ["-w", "com.apple.quarantine", "0081;00000000;Test;", newBundle.path])
        let backup = folder.appendingPathComponent(".Living Brain.previous-x.app", isDirectory: true)
        let workdir = root.appendingPathComponent("work dir", isDirectory: true)
        try FileManager.default.createDirectory(at: workdir, withIntermediateDirectories: true)
        let opener = try makeOpener(in: root)
        let script = try UpdateHelper.writeScript(in: workdir)

        let status = try run("/bin/sh", [script.path, deadPID(), newBundle.path, installed.path,
                                         backup.path, workdir.path, opener.path])
        XCTAssertEqual(status, 0)
        XCTAssertEqual(try String(contentsOf: installed.appendingPathComponent("Contents/Resources/marker")), "new")
        XCTAssertFalse(FileManager.default.fileExists(atPath: newBundle.path), "the new bundle was moved, not copied")
        XCTAssertFalse(FileManager.default.fileExists(atPath: backup.path), "the old bundle is removed on success")
        XCTAssertFalse(FileManager.default.fileExists(atPath: workdir.path), "the work directory is removed on success")
        XCTAssertEqual(try String(contentsOf: root.appendingPathComponent("opened.txt")), installed.path + "\n")
        XCTAssertNotEqual(try run("/usr/bin/xattr", ["-p", "com.apple.quarantine", installed.path]), 0,
                          "quarantine is cleared on the new bundle")
    }

    func testHelperLeavesTheInstalledAppAloneWhenTheNewOneIsMissing() throws {
        let root = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: root) }
        let installed = try makeBundle(in: root, named: "Living Brain.app", marker: "old")
        let workdir = root.appendingPathComponent("work", isDirectory: true)
        try FileManager.default.createDirectory(at: workdir, withIntermediateDirectories: true)
        let opener = try makeOpener(in: root)
        let script = try UpdateHelper.writeScript(in: workdir)
        let status = try run("/bin/sh", [script.path, deadPID(), root.appendingPathComponent("missing.app").path,
                                         installed.path, root.appendingPathComponent("b.app").path,
                                         workdir.path, opener.path])
        XCTAssertNotEqual(status, 0)
        XCTAssertEqual(try String(contentsOf: installed.appendingPathComponent("Contents/Resources/marker")), "old")
        XCTAssertTrue(FileManager.default.fileExists(atPath: root.appendingPathComponent("opened.txt").path),
                      "the old app is relaunched")
    }

    func testHelperRefusesAMissingArgument() throws {
        let dir = try makeTempDir()
        defer { try? FileManager.default.removeItem(at: dir) }
        let script = try UpdateHelper.writeScript(in: dir)
        XCTAssertNotEqual(try run("/bin/sh", [script.path, "", "/a", "/b", "/c", dir.path, "/usr/bin/true"]), 0)
        XCTAssertNotEqual(try run("/bin/sh", [script.path, "12x", "/a", "/b", "/c", dir.path, "/usr/bin/true"]), 0)
    }

    // MARK: Helpers

    private func makeTempDir() throws -> URL {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("updater-tests-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    /// A minimal ad-hoc signed bundle; Contents/Resources/marker says which copy it is.
    private func makeBundle(in folder: URL, named name: String, marker: String) throws -> URL {
        let app = folder.appendingPathComponent(name, isDirectory: true)
        let contents = app.appendingPathComponent("Contents", isDirectory: true)
        try FileManager.default.createDirectory(at: contents.appendingPathComponent("MacOS"), withIntermediateDirectories: true)
        try FileManager.default.createDirectory(at: contents.appendingPathComponent("Resources"), withIntermediateDirectories: true)
        try Data(marker.utf8).write(to: contents.appendingPathComponent("Resources/marker"))
        let plist = """
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0"><dict>
          <key>CFBundleIdentifier</key><string>wiki.livingbrain.test</string>
          <key>CFBundleExecutable</key><string>run</string>
          <key>CFBundlePackageType</key><string>APPL</string>
          <key>CFBundleShortVersionString</key><string>0.0.0</string>
        </dict></plist>
        """
        try Data(plist.utf8).write(to: contents.appendingPathComponent("Info.plist"))
        let exe = contents.appendingPathComponent("MacOS/run")
        try Data("#!/bin/sh\nexit 0\n".utf8).write(to: exe)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: exe.path)
        XCTAssertEqual(try run("/usr/bin/codesign", ["--force", "--sign", "-", app.path]), 0)
        return app
    }

    /// Records its first argument in `root/opened.txt`. Lives in a folder with
    /// a space in its name, like the argument paths.
    private func makeOpener(in root: URL) throws -> URL {
        let folder = root.appendingPathComponent("fake opener", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let url = folder.appendingPathComponent("open")
        let record = root.appendingPathComponent("opened.txt").path
        try Data("#!/bin/sh\nprintf '%s\\n' \"$1\" > '\(record)'\n".utf8).write(to: url)
        try FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: url.path)
        return url
    }

    /// The pid of a process that has already exited: `kill -0` fails at once,
    /// so the helper does not wait.
    private func deadPID() throws -> String {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/true")
        try process.run()
        process.waitUntilExit()
        return String(process.processIdentifier)
    }

    @discardableResult
    private func run(_ executable: String, _ arguments: [String]) throws -> Int32 {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        try process.run()
        process.waitUntilExit()
        return process.terminationStatus
    }
}
