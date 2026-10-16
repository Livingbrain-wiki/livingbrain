import CryptoKit
import Foundation

/// The update logic that needs no window or network: which GitHub release is
/// the newest Mac build, whether it is newer than the running app, and how
/// a downloaded DMG is checked against its published SHA-256.
public enum UpdateRelease {
    /// Tags are `macos-v<semver>`; every other tag in the repository is ignored.
    public static let tagPrefix = "macos-v"
    public static let dmgName = "LivingBrain.dmg"
    public static let checksumName = "LivingBrain.dmg.sha256"
    public static let releasesURL = URL(string: "https://api.github.com/repos/Livingbrain-wiki/livingbrain/releases?per_page=30")!
    public static let releasePageURL = URL(string: "https://github.com/Livingbrain-wiki/livingbrain/releases")!
}

/// major.minor.patch with an optional pre-release tag. Build metadata
/// (`+…`) is accepted and dropped: it never changes which version is newer.
public struct SemVer: Equatable, Comparable, CustomStringConvertible, Sendable {
    public let major: Int
    public let minor: Int
    public let patch: Int
    public let prerelease: String?

    public init(major: Int, minor: Int, patch: Int, prerelease: String? = nil) {
        self.major = major
        self.minor = minor
        self.patch = patch
        self.prerelease = prerelease
    }

    public init?(_ text: String) {
        var core = Substring(text.trimmingCharacters(in: .whitespaces))
        if let plus = core.firstIndex(of: "+") { core = core[..<plus] }
        var pre: String?
        if let dash = core.firstIndex(of: "-") {
            let tag = String(core[core.index(after: dash)...])
            guard !tag.isEmpty else { return nil }
            pre = tag
            core = core[..<dash]
        }
        let parts = core.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 3, let major = Self.number(parts[0]), let minor = Self.number(parts[1]),
              let patch = Self.number(parts[2]) else { return nil }
        self.init(major: major, minor: minor, patch: patch, prerelease: pre)
    }

    /// Digits only, so "1a", "" and "-1" are refused.
    private static func number(_ s: Substring) -> Int? {
        guard !s.isEmpty, s.allSatisfy({ $0.isASCII && $0.isNumber }) else { return nil }
        return Int(s)
    }

    public var description: String {
        let core = "\(major).\(minor).\(patch)"
        return prerelease.map { "\(core)-\($0)" } ?? core
    }

    /// Numeric parts compare as numbers; a release sorts after its pre-releases.
    public static func < (lhs: SemVer, rhs: SemVer) -> Bool {
        if (lhs.major, lhs.minor, lhs.patch) != (rhs.major, rhs.minor, rhs.patch) {
            return (lhs.major, lhs.minor, lhs.patch) < (rhs.major, rhs.minor, rhs.patch)
        }
        switch (lhs.prerelease, rhs.prerelease) {
        case (nil, _): return false
        case (_, nil): return true
        case let (l?, r?): return comparePrerelease(l, r) == .orderedAscending
        }
    }

    private static func comparePrerelease(_ a: String, _ b: String) -> ComparisonResult {
        let x = a.split(separator: ".", omittingEmptySubsequences: false)
        let y = b.split(separator: ".", omittingEmptySubsequences: false)
        for (l, r) in zip(x, y) where l != r {
            if let ln = Int(l), let rn = Int(r) { return ln < rn ? .orderedAscending : .orderedDescending }
            if Int(l) != nil { return .orderedAscending }
            if Int(r) != nil { return .orderedDescending }
            return l < r ? .orderedAscending : .orderedDescending
        }
        return x.count < y.count ? .orderedAscending : (x.count > y.count ? .orderedDescending : .orderedSame)
    }
}

/// A published Mac build with both of its assets.
public struct Release: Equatable, Sendable {
    public let version: SemVer
    public let dmgURL: URL
    public let sha256URL: URL
    public let notes: String
    public let publishedAt: Date?
    public let htmlURL: URL

    public init(version: SemVer, dmgURL: URL, sha256URL: URL, notes: String, publishedAt: Date?, htmlURL: URL) {
        self.version = version
        self.dmgURL = dmgURL
        self.sha256URL = sha256URL
        self.notes = notes
        self.publishedAt = publishedAt
        self.htmlURL = htmlURL
    }
}

// MARK: - GitHub JSON

private struct GitHubRelease: Decodable {
    struct Asset: Decodable {
        let name: String
        let browserDownloadURL: String

        enum CodingKeys: String, CodingKey {
            case name
            case browserDownloadURL = "browser_download_url"
        }
    }

    let tagName: String
    let body: String?
    let draft: Bool?
    let prerelease: Bool?
    let publishedAt: String?
    let htmlURL: String
    let assets: [Asset]?

    enum CodingKeys: String, CodingKey {
        case tagName = "tag_name", body, draft, prerelease
        case publishedAt = "published_at", htmlURL = "html_url", assets
    }
}

/// Decodes one array element, or nothing: a malformed release (or a tag this
/// app does not know) must not hide the valid ones around it.
private struct Lossy<T: Decodable>: Decodable {
    let value: T?
    init(from decoder: Decoder) throws {
        value = try? T(from: decoder)
    }
}

/// The newest eligible release in GitHub's `GET /releases` array: not a draft
/// or a prerelease, tagged `macos-v<semver>` (no pre-release suffix), with
/// both the DMG and its checksum attached over https. Nil when there is none.
public func latestRelease(from json: Data) -> Release? {
    let decoder = JSONDecoder()
    guard let entries = try? decoder.decode([Lossy<GitHubRelease>].self, from: json) else { return nil }
    let candidates: [Release] = entries.compactMap { entry in
        guard let raw = entry.value, raw.draft != true, raw.prerelease != true,
              raw.tagName.hasPrefix(UpdateRelease.tagPrefix),
              let version = SemVer(String(raw.tagName.dropFirst(UpdateRelease.tagPrefix.count))),
              version.prerelease == nil,
              let htmlURL = httpsURL(raw.htmlURL) else { return nil }
        let assets = raw.assets ?? []
        func asset(_ name: String) -> URL? {
            assets.first { $0.name == name }.flatMap { httpsURL($0.browserDownloadURL) }
        }
        guard let dmg = asset(UpdateRelease.dmgName), let sums = asset(UpdateRelease.checksumName) else { return nil }
        return Release(version: version, dmgURL: dmg, sha256URL: sums, notes: raw.body ?? "",
                       publishedAt: parseDate(raw.publishedAt), htmlURL: htmlURL)
    }
    return candidates.reduce(nil) { best, next in
        guard let best else { return next }
        return best.version < next.version ? next : best
    }
}

private func httpsURL(_ text: String) -> URL? {
    guard let url = URL(string: text), url.scheme?.lowercased() == "https", url.host != nil else { return nil }
    return url
}

private func parseDate(_ text: String?) -> Date? {
    guard let text else { return nil }
    let plain = ISO8601DateFormatter()
    if let date = plain.date(from: text) { return date }
    let fractional = ISO8601DateFormatter()
    fractional.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    return fractional.date(from: text)
}

// MARK: - Versions

/// Whether `release` should replace `current` (the running app's
/// CFBundleShortVersionString). A release with a pre-release tag is offered
/// only to a build that is itself a pre-release (so a pre-release line never
/// reaches a stable build); an unreadable current version offers nothing.
public func isNewer(_ release: Release, than current: String) -> Bool {
    guard let installed = SemVer(current) else { return false }
    if release.version.prerelease != nil && installed.prerelease == nil {
        return false
    }
    return release.version > installed
}

/// The auto-check and the menu offer a release unless it is newer than the
/// running app and the user chose to skip exactly that version.
public func shouldOffer(_ release: Release, current: String, skipped: String?) -> Bool {
    isNewer(release, than: current) && skipped != release.version.description
}

/// Rate limits show up as 403 or 429 from the API. Callers retry on the next
/// interval rather than reporting an error.
public func isRateLimited(status: Int) -> Bool {
    status == 403 || status == 429
}

/// The request for the release list. The User-Agent names the app build, as
/// GitHub asks API clients to.
public func releasesRequest(currentVersion: String) -> URLRequest {
    var request = URLRequest(url: UpdateRelease.releasesURL)
    request.timeoutInterval = 30
    request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
    request.setValue("LivingBrain-Mac/\(currentVersion)", forHTTPHeaderField: "User-Agent")
    return request
}

// MARK: - Checksums

/// The first token of a `shasum -a 256` line ("<64 hex>  LivingBrain.dmg"),
/// lower-cased. Nil unless it is exactly 64 hex digits.
public func parseChecksum(_ text: String) -> String? {
    guard let token = text.split(whereSeparator: { $0.isWhitespace }).first else { return nil }
    let hex = token.lowercased()
    guard hex.count == 64, hex.allSatisfy({ $0.isHexDigit }) else { return nil }
    return hex
}

/// SHA-256 of a file, read in chunks so a large DMG is never held whole.
public func sha256(of file: URL) throws -> String {
    let handle = try FileHandle(forReadingFrom: file)
    defer { try? handle.close() }
    var hasher = SHA256()
    while let chunk = try handle.read(upToCount: 1 << 20), !chunk.isEmpty {
        hasher.update(data: chunk)
    }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

/// The first `lines` lines of release notes, with trailing blank lines dropped.
/// The text is shown as written: GitHub's markdown is not rendered here.
public func releaseNotesPreview(_ notes: String, lines: Int = 10) -> String {
    var kept = notes.components(separatedBy: .newlines).prefix(lines).map { $0.trimmingCharacters(in: .whitespaces) }
    while let last = kept.last, last.isEmpty { kept.removeLast() }
    return kept.joined(separator: "\n")
}
