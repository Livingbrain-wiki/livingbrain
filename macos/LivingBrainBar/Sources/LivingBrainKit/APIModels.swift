import Foundation

// Wire shapes of the Living Brain HTTP API, as the server writes them:
// crates/livingbrain-api (pages, search, ask, notes, sources),
// livingbrain-usage, livingbrain-models, livingbrain-tokens. Optional
// fields are decoded leniently so a newer server does not break the app.

/// `GET /v1/pages?limit=` row.
public struct PageSummary: Decodable, Equatable, Sendable, Identifiable, Hashable {
    public var scope: String?
    public var slug: String
    public var entityType: String?
    public var version: Int?
    public var updatedAt: String?
    public var url: String?
    public var id: String { slug }

    enum CodingKeys: String, CodingKey {
        case scope, slug, version, url
        case entityType = "entity_type"
        case updatedAt = "updated_at"
    }

    public var updatedDate: Date? { updatedAt.flatMap(ISODate.parse) }
}

struct PagesEnvelope: Decodable { var pages: [PageSummary] }

/// `GET /v1/pages/{slug}` and the answer to `PUT`.
public struct Page: Decodable, Equatable, Sendable {
    public var slug: String
    public var title: String?
    public var markdown: String
    public var url: String?
    public var version: Int?
    public var entityType: String?
    public var backlinks: [String]
    public var citations: [Citation]

    enum CodingKeys: String, CodingKey {
        case slug, title, markdown, url, version, backlinks, citations
        case entityType = "entity_type"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        slug = try c.decode(String.self, forKey: .slug)
        title = try c.decodeIfPresent(String.self, forKey: .title)
        markdown = try c.decodeIfPresent(String.self, forKey: .markdown) ?? ""
        url = try c.decodeIfPresent(String.self, forKey: .url)
        version = try c.decodeIfPresent(Int.self, forKey: .version)
        entityType = try c.decodeIfPresent(String.self, forKey: .entityType)
        backlinks = try c.decodeIfPresent([String].self, forKey: .backlinks) ?? []
        citations = try c.decodeIfPresent([Citation].self, forKey: .citations) ?? []
    }

    public var displayTitle: String { (title?.isEmpty == false ? title : nil) ?? slug }
}

/// A cited page: in a page's `citations` and an answer's.
public struct Citation: Decodable, Equatable, Sendable, Hashable {
    public var title: String
    public var url: String?
    public var quote: String?

    /// The page slug, read from the page URL (see `SearchResult.slug(fromURL:)`).
    public var slug: String? { url.flatMap(SearchResult.slug(fromURL:)) }
}

/// `GET /v1/search` result.
public struct SearchResult: Decodable, Equatable, Sendable, Identifiable, Hashable {
    public var title: String
    public var url: String
    public var snippet: String?
    public var id: String { url }

    public var slug: String? { Self.slug(fromURL: url) }

    /// The slug a page URL names: the `slug` query item of the web app's page
    /// address (`https://…/wiki?slug=<slug>&scope=<scope>`), which the server
    /// cites, else the last path segment of the older `…/brain/<scope>/<slug>`.
    public static func slug(fromURL url: String) -> String? {
        guard let components = URLComponents(string: url) else { return nil }
        if let slug = components.queryItems?.first(where: { $0.name == "slug" })?.value, !slug.isEmpty {
            return slug
        }
        let last = components.path.split(separator: "/").last.map(String.init)
        return last?.isEmpty == false ? last : nil
    }
}

struct SearchEnvelope: Decodable { var results: [SearchResult] }

/// `POST /v1/ask`.
public struct Answer: Decodable, Equatable, Sendable {
    public var answer: String
    public var citations: [Citation]

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        answer = try c.decodeIfPresent(String.self, forKey: .answer) ?? ""
        citations = try c.decodeIfPresent([Citation].self, forKey: .citations) ?? []
    }

    enum CodingKeys: String, CodingKey { case answer, citations }
}

/// `POST /v1/notes`.
public struct NoteResult: Decodable, Equatable, Sendable {
    public var id: String
    public var url: String?
    public var version: Int?
    public var redacted: Int?
    public var unchanged: Bool?

    /// `<scope>/<slug>` → slug.
    public var slug: String { id.split(separator: "/").last.map(String.init) ?? id }
}

/// `GET /v1/sources` row.
public struct SourceSummary: Decodable, Equatable, Sendable, Identifiable, Hashable {
    public var id: String
    public var kind: String
    public var scope: String?
    public var path: String?
    public var project: String?
    public var held: Bool?
    public var createdAt: String?

    enum CodingKeys: String, CodingKey {
        case id, kind, scope, path, project, held
        case createdAt = "created_at"
    }

    public var createdDate: Date? { createdAt.flatMap(ISODate.parse) }
}

public struct SourcesPage: Decodable, Equatable, Sendable {
    public var sources: [SourceSummary]
    public var nextCursor: String?

    enum CodingKeys: String, CodingKey {
        case sources
        case nextCursor = "next_cursor"
    }
}

/// `GET /v1/sources/projects` row.
public struct SourceProject: Decodable, Equatable, Sendable, Identifiable, Hashable {
    public var project: String
    public var sources: Int
    public var id: String { project }
}

struct SourceProjectsEnvelope: Decodable { var projects: [SourceProject] }

/// `GET /v1/sources/{id}`: the row plus its body.
public struct SourceDetail: Decodable, Equatable, Sendable {
    public var id: String
    public var kind: String
    public var scope: String?
    public var project: String?
    public var author: String?
    public var originRef: String?
    public var createdAt: String?
    public var held: Bool?
    public var sha256: String?
    public var body: String

    enum CodingKeys: String, CodingKey {
        case id, kind, scope, project, author, held, sha256, body
        case originRef = "origin_ref"
        case createdAt = "created_at"
    }

    /// An `agent_log` body is a session record; anything else is text.
    public var sessionRecord: SessionRecord? {
        guard kind == "agent_log" else { return nil }
        if let record = try? JSONDecoder().decode(SessionRecord.self, from: Data(body.utf8)) { return record }
        return try? JSONDecoder().decode(SessionRecord.self, from: Data(SessionShow.repairEscapes(body).utf8))
    }
}

/// `GET /v1/usage/daily?days=` (workspace admins).
public struct Usage: Decodable, Equatable, Sendable {
    public struct Day: Decodable, Equatable, Sendable, Identifiable {
        public var day: String
        public var modelRequests: Int
        public var promptTokens: Int
        public var completionTokens: Int
        public var d1Queries: Int
        public var emailsSent: Int
        public var id: String { day }

        enum CodingKeys: String, CodingKey {
            case day
            case modelRequests = "model_requests"
            case promptTokens = "prompt_tokens"
            case completionTokens = "completion_tokens"
            case d1Queries = "d1_queries"
            case emailsSent = "emails_sent"
        }

        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            day = try c.decode(String.self, forKey: .day)
            modelRequests = try c.decodeIfPresent(Int.self, forKey: .modelRequests) ?? 0
            promptTokens = try c.decodeIfPresent(Int.self, forKey: .promptTokens) ?? 0
            completionTokens = try c.decodeIfPresent(Int.self, forKey: .completionTokens) ?? 0
            d1Queries = try c.decodeIfPresent(Int.self, forKey: .d1Queries) ?? 0
            emailsSent = try c.decodeIfPresent(Int.self, forKey: .emailsSent) ?? 0
        }

        /// `YYYY-MM-DD` as a date at UTC midnight.
        public var date: Date? {
            let f = DateFormatter()
            f.calendar = Calendar(identifier: .gregorian)
            f.locale = Locale(identifier: "en_US_POSIX")
            f.timeZone = TimeZone(identifier: "UTC")
            f.dateFormat = "yyyy-MM-dd"
            return f.date(from: day)
        }
    }

    public struct Totals: Decodable, Equatable, Sendable {
        public var modelRequests: Int
        public var promptTokens: Int
        public var completionTokens: Int
        public var emailsSent: Int
        public var modelUsdMicros: Int?
        public var emailUsdMicros: Int?

        enum CodingKeys: String, CodingKey {
            case modelRequests = "model_requests"
            case promptTokens = "prompt_tokens"
            case completionTokens = "completion_tokens"
            case emailsSent = "emails_sent"
            case modelUsdMicros = "model_usd_micros"
            case emailUsdMicros = "email_usd_micros"
        }

        public init(from decoder: Decoder) throws {
            let c = try decoder.container(keyedBy: CodingKeys.self)
            modelRequests = try c.decodeIfPresent(Int.self, forKey: .modelRequests) ?? 0
            promptTokens = try c.decodeIfPresent(Int.self, forKey: .promptTokens) ?? 0
            completionTokens = try c.decodeIfPresent(Int.self, forKey: .completionTokens) ?? 0
            emailsSent = try c.decodeIfPresent(Int.self, forKey: .emailsSent) ?? 0
            modelUsdMicros = try c.decodeIfPresent(Int.self, forKey: .modelUsdMicros)
            emailUsdMicros = try c.decodeIfPresent(Int.self, forKey: .emailUsdMicros)
        }
    }

    public var workspaceId: String?
    public var days: [Day]
    public var totals: Totals?
    public var monthToDateUsdMicros: Int?
    public var budgetCents: Int?
    /// `ok` or `over`.
    public var status: String?

    enum CodingKeys: String, CodingKey {
        case days, totals, status
        case workspaceId = "workspace_id"
        case monthToDateUsdMicros = "month_to_date_usd_micros"
        case budgetCents = "budget_cents"
    }

    public var monthToDateUSD: Double? { monthToDateUsdMicros.map { Double($0) / 1_000_000 } }
    public var budgetUSD: Double? { budgetCents.map { Double($0) / 100 } }
    public var isOverBudget: Bool { status == "over" }

    /// Month-to-date spend as a fraction of the budget, for a gauge.
    public var budgetFraction: Double? {
        guard let spent = monthToDateUSD, let budget = budgetUSD, budget > 0 else { return nil }
        return spent / budget
    }
}

/// A model connection (`GET /v1/models`, `GET|PUT /v1/models/{role}`).
public struct ModelConnection: Decodable, Equatable, Sendable, Identifiable {
    public var role: String
    public var provider: String
    public var baseUrl: String?
    public var auth: String?
    public var wire: String?
    public var model: String
    /// The key's last characters (`…abcd`); never the key.
    public var key: String?
    public var status: String?
    public var missing: [String]
    public var contextSize: String?
    public var fallbackToManaged: Bool?
    public var checkedAt: String?
    public var id: String { role }

    enum CodingKeys: String, CodingKey {
        case role, provider, auth, wire, model, key, status, missing
        case baseUrl = "base_url"
        case contextSize = "context_size"
        case fallbackToManaged = "fallback_to_managed"
        case checkedAt = "checked_at"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        role = try c.decode(String.self, forKey: .role)
        provider = try c.decodeIfPresent(String.self, forKey: .provider) ?? ""
        baseUrl = try c.decodeIfPresent(String.self, forKey: .baseUrl)
        auth = try c.decodeIfPresent(String.self, forKey: .auth)
        wire = try c.decodeIfPresent(String.self, forKey: .wire)
        model = try c.decodeIfPresent(String.self, forKey: .model) ?? ""
        key = try c.decodeIfPresent(String.self, forKey: .key)
        status = try c.decodeIfPresent(String.self, forKey: .status)
        missing = try c.decodeIfPresent([String].self, forKey: .missing) ?? []
        // The server documents it as a string; accept a number too.
        if let text = try? c.decodeIfPresent(String.self, forKey: .contextSize) {
            contextSize = text
        } else {
            contextSize = (try? c.decodeIfPresent(Int.self, forKey: .contextSize)).flatMap { $0.map(String.init) }
        }
        fallbackToManaged = try c.decodeIfPresent(Bool.self, forKey: .fallbackToManaged)
        checkedAt = try c.decodeIfPresent(String.self, forKey: .checkedAt)
    }
}

/// The roles a model can serve (livingbrain-models handlers.rs).
public enum ModelRole: String, CaseIterable, Identifiable, Sendable {
    case main, triage, research
    public var id: String { rawValue }
    public var title: String {
        switch self {
        case .main: return "Main"
        case .triage: return "Triage"
        case .research: return "Research"
        }
    }
}

/// The body of `PUT /v1/models/{role}`, built as models.js builds it.
public struct ModelConnectRequest: Encodable, Equatable, Sendable {
    public var provider: String
    public var apiKey: String
    public var model: String
    public var fallbackToManaged = false
    public var baseUrl: String?
    public var variables: [String: String]?
    public var wire: String?
    public var auth: String?

    enum CodingKeys: String, CodingKey {
        case provider, model, variables, wire, auth
        case apiKey = "api_key"
        case fallbackToManaged = "fallback_to_managed"
        case baseUrl = "base_url"
    }

    public init(provider: String, apiKey: String, model: String, baseUrl: String? = nil,
                variables: [String: String]? = nil, wire: String? = nil, auth: String? = nil)
    {
        self.provider = provider
        self.apiKey = apiKey
        self.model = model
        let custom = provider == "custom"
        self.baseUrl = baseUrl.map { $0.trimmingCharacters(in: .whitespaces) }
            .flatMap { $0.isEmpty ? nil : String($0.reversed().drop(while: { $0 == "/" }).reversed()) }
        // Catalog providers send their variables and let the server build
        // the URL; a custom endpoint sends its URL, wire and auth.
        self.variables = (variables?.isEmpty == false && self.baseUrl == nil) ? variables : nil
        self.wire = custom ? wire : nil
        self.auth = custom ? auth : nil
    }

    /// What the web form refuses before sending (models.js `connectRequest`).
    public var validationErrors: [String] {
        var errors: [String] = []
        if apiKey.trimmingCharacters(in: .whitespaces).isEmpty { errors.append("Paste the API key for this provider.") }
        if model.trimmingCharacters(in: .whitespaces).isEmpty { errors.append("Enter the model name.") }
        if provider == "custom" {
            if baseUrl == nil { errors.append("Enter the endpoint's base URL.") }
            if wire == nil { errors.append("Choose the API this endpoint speaks.") }
            if auth == nil { errors.append("Choose how the key is sent.") }
        }
        return errors
    }
}

/// `providers.json`, the catalog the web Settings uses (served by the app).
public struct ProviderCatalog: Decodable, Equatable, Sendable {
    public struct Provider: Decodable, Equatable, Sendable, Identifiable, Hashable {
        public struct Variable: Decodable, Equatable, Sendable, Hashable {
            public var name: String
            public var label: String?
            public var placeholder: String?
        }

        public var id: String
        public var name: String
        public var baseUrl: String?
        public var auth: String?
        public var wire: String?
        public var site: String?
        public var variables: [Variable]?

        enum CodingKeys: String, CodingKey {
            case id, name, auth, wire, site, variables
            case baseUrl = "base_url"
        }
    }

    public var builtin: [Provider]
    public var providers: [Provider]

    /// Built-ins first, then the catalog by name.
    public var all: [Provider] {
        builtin + providers.filter { p in !builtin.contains { $0.id == p.id } }
            .sorted { $0.name.localizedCaseInsensitiveCompare($1.name) == .orderedAscending }
    }
}

/// `GET /v1/tokens` row. The token value is never listed.
public struct TokenSummary: Decodable, Equatable, Sendable, Identifiable {
    public var prefix: String
    public var name: String
    public var scopes: [String]?
    public var createdAt: String?
    public var id: String { prefix }

    enum CodingKeys: String, CodingKey {
        case prefix, name, scopes
        case createdAt = "created_at"
    }

    public var createdDate: Date? { createdAt.flatMap(ISODate.parse) }
    public var scopeLabel: String { scopes.map { $0.joined(separator: ", ") } ?? "Full access" }
}

struct TokensEnvelope: Decodable { var tokens: [TokenSummary] }

/// `POST /v1/device-auth/code`.
public struct DeviceAuthorization: Decodable, Equatable, Sendable {
    public var deviceCode: String
    public var userCode: String
    public var verificationUri: String
    public var verificationUriComplete: String?
    public var interval: Int?
    public var expiresIn: Int?

    enum CodingKeys: String, CodingKey {
        case interval
        case deviceCode = "device_code"
        case userCode = "user_code"
        case verificationUri = "verification_uri"
        case verificationUriComplete = "verification_uri_complete"
        case expiresIn = "expires_in"
    }

    public var openURL: URL? { URL(string: verificationUriComplete ?? verificationUri) }
}

/// One poll of `POST /v1/device-auth/token`.
public enum DevicePoll: Equatable, Sendable {
    case pending
    case slowDown
    case issued(String)
}

extension DevicePoll: CustomStringConvertible {
    /// Never prints the token.
    public var description: String {
        switch self {
        case .pending: return "pending"
        case .slowDown: return "slow_down"
        case .issued: return "issued(<redacted>)"
        }
    }
}

// MARK: Who the credential is (GET /v1/me, ADR 0004)

/// `GET /v1/me`: the workspace, the principal and the credential a bearer
/// token speaks for — the app's "connected as". Never carries a secret.
public struct Me: Decodable, Equatable, Sendable {
    public struct Workspace: Decodable, Equatable, Sendable {
        public var id: String
        public var name: String
    }

    public struct Principal: Decodable, Equatable, Sendable {
        public struct Owner: Decodable, Equatable, Sendable {
            public var id: String
            public var name: String
        }

        /// `member` or `agent`.
        public var kind: String
        public var id: String
        public var name: String
        public var role: String?
        public var onBehalfOf: Owner?

        enum CodingKeys: String, CodingKey {
            case kind, id, name, role
            case onBehalfOf = "on_behalf_of"
        }
    }

    public struct Credential: Decodable, Equatable, Sendable {
        /// `session`, `personal_access_token` or `agent_key`.
        public var kind: String
        public var prefix: String?
        public var scopes: [String]?
    }

    public var workspace: Workspace
    public var principal: Principal
    public var credential: Credential

    /// "Ada · owner", or "CI bot (agent for Ada)".
    public var principalLabel: String {
        let name = principal.name.isEmpty ? principal.id : principal.name
        if principal.kind == "agent" {
            let owner = principal.onBehalfOf.map { $0.name.isEmpty ? $0.id : $0.name }
            return owner.map { "\(name) (agent for \($0))" } ?? "\(name) (agent)"
        }
        return [name, principal.role].compactMap { $0 }.joined(separator: " · ")
    }

    /// "Acme (ws_…)".
    public var workspaceLabel: String {
        workspace.name.isEmpty ? workspace.id : "\(workspace.name) (\(workspace.id))"
    }
}
