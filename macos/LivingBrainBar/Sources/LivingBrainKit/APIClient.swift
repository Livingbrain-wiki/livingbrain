import Foundation

/// Errors from the API, read the way the CLI reads them (api.rs
/// `api_error`): `error`, then `message`, then a problem document's
/// `title: detail`.
public enum APIError: Error, Equatable, LocalizedError, Sendable {
    /// 401: no token, or the server refused it.
    case unauthorized(String)
    /// 403: e.g. usage is for workspace admins.
    case forbidden(String)
    case notFound(String)
    /// 409: a page moved since it was read.
    case conflict(String)
    case http(status: Int, message: String)
    case transport(String)
    case decoding(String)

    public var errorDescription: String? {
        switch self {
        case let .unauthorized(m), let .forbidden(m), let .notFound(m), let .conflict(m),
             let .transport(m), let .decoding(m):
            return m
        case let .http(_, m):
            return m
        }
    }

    public var isUnauthorized: Bool {
        if case .unauthorized = self { return true }
        return false
    }

    /// Map a non-2xx response.
    public static func from(status: Int, body: Data) -> APIError {
        let object = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
        func field(_ name: String) -> String? {
            (object[name] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines).nilIfEmpty
        }
        let problem: String? = {
            switch (field("title"), field("detail")) {
            case let (title?, detail?): return "\(title): \(detail)"
            case let (nil, detail?): return detail
            case let (title?, nil): return title
            default: return nil
            }
        }()
        let raw = String(decoding: body, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
        var message = field("error") ?? field("message") ?? problem ?? (raw.isEmpty || raw.count > 300 ? nil : raw)
            ?? "The server answered HTTP \(status)"
        message = Scrub.text(message)
        switch status {
        case 401: return .unauthorized(message)
        case 403: return .forbidden(message)
        case 404: return .notFound(message)
        case 409: return .conflict(message)
        default: return .http(status: status, message: message)
        }
    }
}

extension String {
    var nilIfEmpty: String? { isEmpty ? nil : self }
}

/// The HTTP layer, swappable in tests.
public protocol HTTPTransport: Sendable {
    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse)
}

public struct URLSessionTransport: HTTPTransport {
    let session: URLSession

    public init(session: URLSession = .shared) {
        self.session = session
    }

    public func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else {
            throw APIError.transport("The server sent a response that is not HTTP")
        }
        return (data, http)
    }
}

/// The Living Brain API over HTTPS with `Authorization: Bearer`. The token
/// is only ever put in that header: it is not logged, stored here, or shown.
public struct APIClient: Sendable {
    public static let deviceClientId = "livingbrain-cli"

    public let baseURL: URL
    let token: String?
    let transport: HTTPTransport

    public init(baseURL: URL, token: String?, transport: HTTPTransport = URLSessionTransport()) {
        self.baseURL = baseURL
        self.token = token
        self.transport = transport
    }

    public var hasToken: Bool { token?.isEmpty == false }

    // MARK: Requests

    func url(_ path: String, query: [(String, String?)] = []) -> URL {
        var components = URLComponents(url: baseURL, resolvingAgainstBaseURL: false)!
        let base = components.path.hasSuffix("/") ? String(components.path.dropLast()) : components.path
        components.percentEncodedPath = base + path
        let items = query.compactMap { name, value in value.map { URLQueryItem(name: name, value: $0) } }
        components.queryItems = items.isEmpty ? nil : items
        return components.url!
    }

    func request(_ method: String, _ path: String, query: [(String, String?)] = [], json: Encodable? = nil,
                 form: [(String, String)]? = nil, authorized: Bool = true) throws -> URLRequest
    {
        var request = URLRequest(url: url(path, query: query))
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        request.timeoutInterval = 60
        if authorized {
            guard let token, !token.isEmpty else { throw APIError.unauthorized("Sign in to Living Brain first.") }
            request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        if let json {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(AnyEncodable(json))
        } else if let form {
            request.setValue("application/x-www-form-urlencoded", forHTTPHeaderField: "Content-Type")
            request.httpBody = Data(Self.formEncode(form).utf8)
        }
        return request
    }

    static func formEncode(_ pairs: [(String, String)]) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        return pairs.map { name, value in
            "\(name.addingPercentEncoding(withAllowedCharacters: allowed) ?? name)=\(value.addingPercentEncoding(withAllowedCharacters: allowed) ?? value)"
        }.joined(separator: "&")
    }

    func send(_ request: URLRequest) async throws -> Data {
        let data: Data
        let response: HTTPURLResponse
        do {
            (data, response) = try await transport.send(request)
        } catch let error as APIError {
            throw error
        } catch {
            throw APIError.transport("Could not reach Living Brain: \(error.localizedDescription)")
        }
        guard (200..<300).contains(response.statusCode) else {
            throw APIError.from(status: response.statusCode, body: data)
        }
        return data
    }

    func decode<T: Decodable>(_ type: T.Type, _ data: Data) throws -> T {
        do {
            return try JSONDecoder().decode(type, from: data)
        } catch {
            throw APIError.decoding("The server's response was not what this app expected (\(type)).")
        }
    }

    func get<T: Decodable>(_ type: T.Type, _ path: String, query: [(String, String?)] = []) async throws -> T {
        try decode(type, await send(request("GET", path, query: query)))
    }

    static func segment(_ value: String) -> String {
        var allowed = CharacterSet.alphanumerics
        allowed.insert(charactersIn: "-._~")
        return value.addingPercentEncoding(withAllowedCharacters: allowed) ?? value
    }

    // MARK: Pages, search, ask, notes

    public func pages(limit: Int = 200) async throws -> [PageSummary] {
        try await get(PagesEnvelope.self, "/v1/pages", query: [("limit", String(limit))]).pages
    }

    public func page(_ slug: String) async throws -> Page {
        try await get(Page.self, "/v1/pages/\(Self.segment(slug))")
    }

    /// `PUT /v1/pages/{slug}`: `baseVersion` is the version that was read;
    /// nil creates the page. A stale base is `APIError.conflict`.
    public func savePage(_ slug: String, markdown: String, baseVersion: Int?, title: String? = nil) async throws -> Page {
        struct Body: Encodable {
            var markdown: String
            var base_version: Int?
            var title: String?

            func encode(to encoder: Encoder) throws {
                var c = encoder.container(keyedBy: CodingKeys.self)
                try c.encode(markdown, forKey: .markdown)
                try c.encode(base_version, forKey: .base_version) // null creates
                try c.encodeIfPresent(title, forKey: .title)
            }

            enum CodingKeys: String, CodingKey { case markdown, base_version, title }
        }
        let request = try request("PUT", "/v1/pages/\(Self.segment(slug))",
                                  json: Body(markdown: markdown, base_version: baseVersion, title: title))
        return try decode(Page.self, await send(request))
    }

    public func search(_ query: String, project: String? = nil, limit: Int = 20) async throws -> [SearchResult] {
        try await get(SearchEnvelope.self, "/v1/search",
                      query: [("q", query), ("project", project?.nilIfEmpty), ("limit", String(limit))]).results
    }

    /// `POST /v1/ask`. A member who runs their brain jobs on their own device
    /// (Settings → "Run brain jobs with my subscription") gets `202 pending`:
    /// their runner answers. This polls the job until it ends, then asks
    /// again, which reads the runner's answer. `onWait` runs once when the
    /// wait starts; after `maxWait` seconds it gives up with an error naming
    /// the runner. `pollInterval` overrides the server's `retry_after` (tests).
    public func ask(
        _ question: String,
        project: String? = nil,
        onWait: (@Sendable () -> Void)? = nil,
        maxWait: TimeInterval = 300,
        pollInterval: TimeInterval? = nil
    ) async throws -> Answer {
        struct Body: Encodable { var question: String; var project: String? }
        let body = Body(question: question, project: project?.nilIfEmpty)
        let first = try decode(Answer.self, await send(request("POST", "/v1/ask", json: body)))
        guard first.status == "pending" else { return first }
        guard let poll = first.poll, poll.hasPrefix("/v1/model-jobs/") else {
            throw APIError.decoding("The server named no job to wait for.")
        }
        onWait?()
        let pause = pollInterval ?? TimeInterval(min(max(first.retryAfter ?? 2, 1), 30))
        let started = Date()
        while true {
            if Date().timeIntervalSince(started) > maxWait {
                throw APIError.transport(
                    "Your device did not answer in time — is “Run Brain Jobs with My Subscription” on?"
                )
            }
            try await Task.sleep(nanoseconds: UInt64(pause * 1_000_000_000))
            let job = try? decode(JobState.self, await send(request("GET", poll)))
            if job?.status != "queued" && job?.status != "claimed" {
                return try decode(Answer.self, await send(request("POST", "/v1/ask", json: body)))
            }
        }
    }

    public func createNote(body: String, title: String? = nil, project: String? = nil, slug: String? = nil) async throws -> NoteResult {
        struct Body: Encodable { var body: String; var title: String?; var project: String?; var slug: String? }
        let request = try request("POST", "/v1/notes", json: Body(
            body: body, title: title?.nilIfEmpty, project: project?.nilIfEmpty, slug: slug?.nilIfEmpty
        ))
        return try decode(NoteResult.self, await send(request))
    }

    // MARK: Sources

    public func sources(kind: String? = nil, project: String? = nil, limit: Int = 50, cursor: String? = nil) async throws -> SourcesPage {
        try await get(SourcesPage.self, "/v1/sources", query: [
            ("kind", kind), ("project", project?.nilIfEmpty), ("limit", String(limit)), ("cursor", cursor),
        ])
    }

    public func sourceProjects(kind: String? = nil) async throws -> [SourceProject] {
        try await get(SourceProjectsEnvelope.self, "/v1/sources/projects", query: [("kind", kind)]).projects
    }

    public func source(_ id: String) async throws -> SourceDetail {
        try await get(SourceDetail.self, "/v1/sources/\(Self.segment(id))")
    }

    // MARK: Usage, models, tokens

    public func usage(days: Int = 14) async throws -> Usage {
        try await get(Usage.self, "/v1/usage/daily", query: [("days", String(min(max(days, 1), 31)))])
    }

    /// `GET /v1/settings/budgets`: the workspace's budgets and limits, read-only.
    public func budgets() async throws -> BudgetSettings {
        try await get(BudgetSettings.self, "/v1/settings/budgets")
    }

    public func models() async throws -> [ModelConnection] {
        try await get([ModelConnection].self, "/v1/models")
    }

    public func connectModel(role: ModelRole, _ body: ModelConnectRequest) async throws -> ModelConnection {
        let request = try request("PUT", "/v1/models/\(role.rawValue)", json: body)
        return try decode(ModelConnection.self, await send(request))
    }

    public func disconnectModel(role: ModelRole) async throws {
        _ = try await send(request("DELETE", "/v1/models/\(role.rawValue)"))
    }

    /// `POST /v1/models/discover`: the models an endpoint offers; nothing is stored.
    public func discoverModels(_ body: ModelConnectRequest) async throws -> [String] {
        struct Body: Encodable {
            var provider: String
            var api_key: String
            var base_url: String?
            var variables: [String: String]?
            var wire: String?
            var auth: String?
        }
        struct Envelope: Decodable { var models: [String] }
        let request = try request("POST", "/v1/models/discover", json: Body(
            provider: body.provider, api_key: body.apiKey, base_url: body.baseUrl,
            variables: body.variables, wire: body.wire, auth: body.auth
        ))
        return try decode(Envelope.self, await send(request)).models
    }

    /// The provider catalog the web app ships (`/assets/providers.json`).
    public func providerCatalog() async throws -> ProviderCatalog {
        try decode(ProviderCatalog.self, await send(request("GET", "/assets/providers.json", authorized: false)))
    }

    /// `GET /v1/me`: who the saved token speaks for, in which workspace.
    public func me() async throws -> Me {
        try await get(Me.self, "/v1/me")
    }

    public func tokens() async throws -> [TokenSummary] {
        try await get(TokensEnvelope.self, "/v1/tokens").tokens
    }

    public func revokeToken(prefix: String) async throws {
        _ = try await send(request("DELETE", "/v1/tokens/\(Self.segment(prefix))"))
    }

    // MARK: Device sign-in (RFC 8628), as the CLI does it

    public func deviceAuthorize() async throws -> DeviceAuthorization {
        struct Body: Encodable { var client_id: String }
        let request = try request("POST", "/v1/device-auth/code",
                                  json: Body(client_id: Self.deviceClientId), authorized: false)
        return try decode(DeviceAuthorization.self, await send(request))
    }

    public func devicePoll(deviceCode: String) async throws -> DevicePoll {
        let request = try request("POST", "/v1/device-auth/token", form: [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", deviceCode),
            ("client_id", Self.deviceClientId),
        ], authorized: false)
        let (data, response): (Data, HTTPURLResponse)
        do {
            (data, response) = try await transport.send(request)
        } catch {
            throw APIError.transport("Could not reach Living Brain: \(error.localizedDescription)")
        }
        let object = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any] ?? [:]
        if (200..<300).contains(response.statusCode), let token = object["access_token"] as? String, !token.isEmpty {
            return .issued(token)
        }
        if response.statusCode == 400, let code = object["error"] as? String {
            switch code {
            case "authorization_pending": return .pending
            case "slow_down": return .slowDown
            case "expired_token": throw APIError.http(status: 400, message: "The sign-in code expired. Start again.")
            case "access_denied": throw APIError.http(status: 400, message: "Sign-in was declined.")
            default: break
            }
        }
        throw APIError.from(status: response.statusCode, body: data)
    }

    /// Seconds between polls: the server's interval, never under RFC 8628's five.
    public static func pollInterval(_ server: Int?) -> Int { max(server ?? 5, 5) }
}

/// Type-erased `Encodable` so the request builder takes any body.
struct AnyEncodable: Encodable {
    let value: Encodable
    init(_ value: Encodable) { self.value = value }
    func encode(to encoder: Encoder) throws { try value.encode(to: encoder) }
}

/// Serves recorded responses from a directory, for UI checks and demos
/// without a server or a token: `GET /v1/pages/q3-plan` reads
/// `<dir>/GET_v1_pages_q3-plan.json`, falling back to
/// `<dir>/GET_v1_pages.json`-style parents is not attempted. A missing file
/// is a 404.
public struct FixtureTransport: HTTPTransport {
    public let directory: URL

    public init(directory: URL) {
        self.directory = directory
    }

    public static func fileName(method: String, path: String) -> String {
        let flat = path.split(separator: "/").joined(separator: "_")
        return "\(method)_\(flat).json"
    }

    public func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        let name = Self.fileName(method: request.httpMethod ?? "GET", path: request.url?.path ?? "/")
        let url = directory.appendingPathComponent(name)
        let found = try? Data(contentsOf: url)
        let status = found == nil ? 404 : 200
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
        return (found ?? Data(#"{"error":"no fixture \#(name)"}"#.utf8), response)
    }
}
