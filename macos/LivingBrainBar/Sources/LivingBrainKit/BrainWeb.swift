import Foundation

/// The rules of the 3D brain view: the page is the web app's own `/brain`
/// front end, loaded through a custom scheme so the app can attach its
/// credential natively. Pure, so the tests pin the policy without a web view.
///
/// Auth: the web app signs in with an HttpOnly cookie, which a native app
/// cannot hold for that origin. `GET /v1/graph` also accepts the bearer token
/// the app keeps in the keychain, so the scheme handler adds
/// `Authorization: Bearer` to `GET`/`HEAD` requests under `/v1/` and nothing
/// else. The token never appears in a URL, a page, or JavaScript-visible storage.
public enum BrainWeb {
    /// The scheme the brain page is loaded under (`lbbrain://livingbrain/brain`).
    public static let scheme = "lbbrain"
    public static let host = "livingbrain"
    /// The page the view shows, as the web app serves it.
    public static let pagePath = "/brain"
    /// The map endpoint the page reads; the preflight checks it before loading.
    public static let graphPath = "/v1/graph"

    public static var entryURL: URL {
        URL(string: "\(scheme)://\(host)\(pagePath)")!
    }

    /// How a request the web view makes is sent upstream.
    public enum Upstream: Equatable, Sendable {
        /// Static front-end files: no credential.
        case anonymous
        /// A read under `/v1/`: the app's bearer token is attached.
        case bearer
        /// Anything that writes, or any method on `/v1/` the map does not need.
        case refused
    }

    public static func upstream(method: String, path: String) -> Upstream {
        let isRead = method.uppercased() == "GET" || method.uppercased() == "HEAD"
        guard isRead else { return .refused }
        return path == "/v1" || path.hasPrefix("/v1/") ? .bearer : .anonymous
    }

    /// The server URL a request on this scheme maps to, under the server's origin
    /// (its path prefix, if any, kept). Nil for a URL on another scheme.
    public static func serverURL(for url: URL, server: URL) -> URL? {
        guard url.scheme == scheme,
              var components = URLComponents(url: server, resolvingAgainstBaseURL: false)
        else { return nil }
        let base = components.percentEncodedPath.hasSuffix("/")
            ? String(components.percentEncodedPath.dropLast())
            : components.percentEncodedPath
        components.percentEncodedPath = base + url.path
        components.percentEncodedQuery = url.query(percentEncoded: true)
        return components.url
    }

    /// Where a navigation in the web view goes.
    public enum Navigation: Equatable, Sendable {
        /// The brain page (or `about:blank`), in the web view.
        case allow
        /// Another page or site: open it in the default browser.
        case browser(URL)
        /// Nothing the view should follow.
        case cancel
    }

    public static func navigation(to url: URL, server: URL) -> Navigation {
        if url.scheme == scheme {
            if url.host == host, url.path == pagePath || url.path == pagePath + ".html" {
                return .allow
            }
            if let mapped = serverURL(for: url, server: server) { return .browser(mapped) }
            return .cancel
        }
        if url.absoluteString == "about:blank" { return .allow }
        if let scheme = url.scheme?.lowercased(), ["http", "https", "mailto"].contains(scheme) {
            return .browser(url)
        }
        return .cancel
    }

    /// Whether two URLs share scheme, host and port: the only redirect the
    /// scheme handler follows.
    public static func sameOrigin(_ lhs: URL, _ rhs: URL) -> Bool {
        lhs.scheme?.lowercased() == rhs.scheme?.lowercased()
            && lhs.host?.lowercased() == rhs.host?.lowercased()
            && effectivePort(lhs) == effectivePort(rhs)
    }

    private static func effectivePort(_ url: URL) -> Int? {
        if let port = url.port { return port }
        switch url.scheme?.lowercased() {
        case "https": return 443
        case "http": return 80
        default: return nil
        }
    }

    /// The page's session check. The web app answers it from the cookie only,
    /// so with a bearer token it would always read "signed out" and hide the
    /// map. The handler answers this one GET from `/v1/me` (the bearer-readable
    /// identity) and reshapes it into the cookie route's form.
    public static let sessionPath = "/v1/workspaces/me"
    public static let bearerIdentityPath = "/v1/me"

    /// The path a request is sent upstream on: the session check's bearer
    /// equivalent, otherwise the path itself.
    public static func upstreamPath(method: String, path: String) -> String {
        method.uppercased() == "GET" && path == sessionPath ? bearerIdentityPath : path
    }

    /// `/v1/me` as the cookie route's `/v1/workspaces/me`: `{workspace, member,
    /// is_owner}`. Nil when the token is not a member's (an agent key) or the
    /// body is not what was expected. `is_owner` is reported as false: the
    /// server still decides every write, so the page only loses owner-only
    /// controls, never gains any.
    public static func sessionShim(fromMe data: Data) -> Data? {
        guard let me = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let workspace = me["workspace"] as? [String: Any],
              let principal = me["principal"] as? [String: Any],
              principal["kind"] as? String == "member"
        else { return nil }
        var member: [String: Any] = [:]
        for key in ["id", "name", "display_name", "avatar_url", "role"] {
            if let value = principal[key], !(value is NSNull) { member[key] = value }
        }
        return try? JSONSerialization.data(withJSONObject: [
            "workspace": workspace,
            "member": member,
            "is_owner": false,
        ] as [String: Any])
    }

    /// Response headers the scheme handler passes to the web view. The body
    /// arrives already decoded, so the encoding and length of the wire are
    /// dropped; cookies are never stored.
    public static func passesHeader(_ name: String) -> Bool {
        let lower = name.lowercased()
        return !["set-cookie", "content-encoding", "content-length", "transfer-encoding", "connection", "keep-alive"]
            .contains(lower)
    }
}
