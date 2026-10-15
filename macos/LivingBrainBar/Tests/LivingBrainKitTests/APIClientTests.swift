import Foundation
import XCTest
@testable import LivingBrainKit

func apiFixture(_ name: String) throws -> Data {
    let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "json", subdirectory: "Fixtures/api"))
    return try Data(contentsOf: url)
}

/// Records requests and answers from a table of `METHOD path` → (status, body).
final class MockTransport: HTTPTransport, @unchecked Sendable {
    private let lock = NSLock()
    private var routes: [String: (Int, Data)] = [:]
    private var queued: [String: [(Int, Data)]] = [:]
    private(set) var requests: [URLRequest] = []

    func on(_ method: String, _ path: String, status: Int = 200, _ body: Data) {
        lock.lock(); routes["\(method) \(path)"] = (status, body); lock.unlock()
    }

    /// Answers the next request to `METHOD path` once, before `on`'s answer.
    func then(_ method: String, _ path: String, status: Int = 200, _ body: Data) {
        lock.lock(); queued["\(method) \(path)", default: []].append((status, body)); lock.unlock()
    }

    func send(_ request: URLRequest) async throws -> (Data, HTTPURLResponse) {
        lock.lock()
        requests.append(request)
        let key = "\(request.httpMethod ?? "GET") \(request.url!.path)"
        let (status, body): (Int, Data)
        if var waiting = queued[key], !waiting.isEmpty {
            (status, body) = waiting.removeFirst()
            queued[key] = waiting
        } else {
            (status, body) = routes[key] ?? (404, Data(#"{"error":"no route"}"#.utf8))
        }
        lock.unlock()
        let response = HTTPURLResponse(url: request.url!, statusCode: status, httpVersion: nil, headerFields: nil)!
        return (body, response)
    }

    var last: URLRequest? { lock.lock(); defer { lock.unlock() }; return requests.last }

    func lastJSON() throws -> [String: Any] {
        let body = try XCTUnwrap(last?.httpBody)
        return try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
    }
}

final class APIClientTests: XCTestCase {
    let base = URL(string: "https://app.livingbrain.wiki")!
    var mock: MockTransport!
    var client: APIClient!

    override func setUp() {
        mock = MockTransport()
        client = APIClient(baseURL: base, token: "tok-secret-123", transport: mock)
    }

    func testBearerHeaderAndNoTokenAnywhereElse() async throws {
        mock.on("GET", "/v1/pages", try apiFixture("pages"))
        let pages = try await client.pages(limit: 50)
        XCTAssertEqual(pages.map(\.slug), ["q3-plan", "kestrel"])
        XCTAssertEqual(pages[0].entityType, "decision")
        XCTAssertNotNil(pages[0].updatedDate)
        let request = try XCTUnwrap(mock.last)
        XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer tok-secret-123")
        XCTAssertEqual(request.url?.absoluteString, "https://app.livingbrain.wiki/v1/pages?limit=50")
        XCTAssertNil(request.value(forHTTPHeaderField: "Origin"), "token routes refuse a cross-site Origin")
    }

    func testNoTokenIsRefusedBeforeTheNetwork() async {
        let anonymous = APIClient(baseURL: base, token: nil, transport: mock)
        do {
            _ = try await anonymous.pages()
            XCTFail("expected unauthorized")
        } catch let error as APIError {
            XCTAssertTrue(error.isUnauthorized)
        } catch { XCTFail("\(error)") }
        XCTAssertTrue(mock.requests.isEmpty)
    }

    func testPageDecodesAndSlugIsPercentEncoded() async throws {
        mock.on("GET", "/v1/pages/q3-plan", try apiFixture("page"))
        let page = try await client.page("q3-plan")
        XCTAssertEqual(page.displayTitle, "Q3 plan")
        XCTAssertEqual(page.version, 3)
        XCTAssertEqual(page.backlinks, ["kestrel"])
        XCTAssertEqual(page.citations.first?.slug, "kestrel")
        XCTAssertEqual(APIClient.segment("a b/c"), "a%20b%2Fc")
    }

    func testSaveSendsBaseVersionAndNullCreates() async throws {
        mock.on("PUT", "/v1/pages/q3-plan", try apiFixture("page"))
        _ = try await client.savePage("q3-plan", markdown: "# x", baseVersion: 3)
        var body = try mock.lastJSON()
        XCTAssertEqual(body["markdown"] as? String, "# x")
        XCTAssertEqual(body["base_version"] as? Int, 3)
        XCTAssertNil(body["title"])
        _ = try await client.savePage("q3-plan", markdown: "", baseVersion: nil, title: "New")
        body = try mock.lastJSON()
        XCTAssertTrue(body["base_version"] is NSNull, "a create sends base_version: null")
        XCTAssertEqual(body["title"] as? String, "New")
    }

    func testConflictIsReadFromTheProblemDocument() async throws {
        mock.on("PUT", "/v1/pages/q3-plan", status: 409, try apiFixture("conflict"))
        do {
            _ = try await client.savePage("q3-plan", markdown: "x", baseVersion: 2)
            XCTFail("expected a conflict")
        } catch let APIError.conflict(message) {
            XCTAssertTrue(message.hasPrefix("The page moved: nothing was written"), message)
        }
    }

    func testErrorShapes() {
        XCTAssertEqual(APIError.from(status: 401, body: Data(#"{"type":"x","title":"Bearer token required","status":401}"#.utf8)),
                       .unauthorized("Bearer token required"))
        XCTAssertEqual(APIError.from(status: 403, body: Data(#"{"error":"admins only"}"#.utf8)), .forbidden("admins only"))
        XCTAssertEqual(APIError.from(status: 500, body: Data()), .http(status: 500, message: "The server answered HTTP 500"))
        XCTAssertEqual(APIError.from(status: 502, body: Data("bad gateway".utf8)), .http(status: 502, message: "bad gateway"))
        if case let .http(_, message) = APIError.from(status: 400, body: Data(#"{"message":"Bearer abcdefgh123 leaked"}"#.utf8)) {
            XCTAssertFalse(message.contains("abcdefgh123"), "error text is scrubbed")
        } else { XCTFail() }
    }

    func testSearchAskAndNotes() async throws {
        mock.on("GET", "/v1/search", try apiFixture("search"))
        let results = try await client.search("kestrel", project: "atlas", limit: 5)
        XCTAssertEqual(results.first?.slug, "q3-plan")
        // The web app's page address, which the server cites now.
        XCTAssertEqual(SearchResult.slug(fromURL: "https://app.livingbrain.wiki/wiki?slug=q3-plan&scope=user-01HX"), "q3-plan")
        XCTAssertEqual(SearchResult.slug(fromURL: "https://app.livingbrain.wiki/wiki?slug=coding-agents%2Frepo%2Fs1&scope=shared"),
                       "coding-agents/repo/s1")
        let query = URLComponents(url: mock.last!.url!, resolvingAgainstBaseURL: false)!.queryItems!
        XCTAssertEqual(query.map(\.name), ["q", "project", "limit"])
        XCTAssertEqual(query[0].value, "kestrel")

        mock.on("POST", "/v1/ask", try apiFixture("ask"))
        let answer = try await client.ask("what first?", project: "")
        XCTAssertEqual(answer.citations.first?.quote, "Ship kestrel first.")
        let askBody = try mock.lastJSON()
        XCTAssertEqual(askBody["question"] as? String, "what first?")
        XCTAssertNil(askBody["project"], "an empty project is left out")

        mock.on("POST", "/v1/notes", try apiFixture("note"))
        let note = try await client.createNote(body: "remember", title: "T", project: "atlas")
        XCTAssertEqual(note.slug, "note-3fa2")
        let noteBody = try mock.lastJSON()
        XCTAssertEqual(noteBody["project"] as? String, "atlas")
        XCTAssertEqual(mock.last?.value(forHTTPHeaderField: "Content-Type"), "application/json")
    }

    func testAskWaitsForTheMembersOwnRunner() async throws {
        mock.then("POST", "/v1/ask", status: 202, Data(
            #"{"status":"pending","job":"mj-1","poll":"/v1/model-jobs/mj-1","retry_after":2,"citations":[]}"#.utf8
        ))
        mock.on("POST", "/v1/ask", Data(#"{"answer":"In Postgres [1].","citations":[]}"#.utf8))
        mock.then("GET", "/v1/model-jobs/mj-1", Data(#"{"id":"mj-1","status":"claimed"}"#.utf8))
        mock.on("GET", "/v1/model-jobs/mj-1", Data(#"{"id":"mj-1","status":"done"}"#.utf8))
        let waited = Waited()
        let answer = try await client.ask("where?", onWait: { waited.mark() }, pollInterval: 0.01)
        XCTAssertEqual(answer.answer, "In Postgres [1].")
        XCTAssertEqual(waited.count, 1)
        XCTAssertEqual(mock.requests.map { "\($0.httpMethod!) \($0.url!.path)" }, [
            "POST /v1/ask", "GET /v1/model-jobs/mj-1", "GET /v1/model-jobs/mj-1", "POST /v1/ask",
        ])
    }

    func testAskNeverFollowsAPollOutsideTheJobRoutes() async {
        mock.on("POST", "/v1/ask", status: 202, Data(#"{"status":"pending","poll":"https://evil.example/x"}"#.utf8))
        do {
            _ = try await client.ask("where?", pollInterval: 0.01)
            XCTFail("an off-route poll must be refused")
        } catch {}
        XCTAssertEqual(mock.requests.count, 1)
    }

    func testSources() async throws {
        mock.on("GET", "/v1/sources", try apiFixture("sources"))
        let page = try await client.sources(kind: "agent_log", project: "atlas")
        XCTAssertEqual(page.sources.first?.project, "atlas")
        XCTAssertEqual(page.nextCursor, "01HZZZBBBBBBBBBBBBBBBBBBB")
        XCTAssertTrue(mock.last!.url!.query!.contains("kind=agent_log"))

        mock.on("GET", "/v1/sources/projects", try apiFixture("source-projects"))
        let projects = try await client.sourceProjects(kind: "agent_log")
        XCTAssertEqual(projects.map(\.sources), [12, 3])

        mock.on("GET", "/v1/sources/01HZZZBBBBBBBBBBBBBBBBBBB", try apiFixture("source"))
        let source = try await client.source("01HZZZBBBBBBBBBBBBBBBBBBB")
        XCTAssertEqual(source.sessionRecord?.turns.first?.text, "hi")
        XCTAssertEqual(source.sessionRecord?.branch, "main")
    }

    func testUsage() async throws {
        mock.on("GET", "/v1/usage/daily", try apiFixture("usage"))
        let usage = try await client.usage(days: 90)
        XCTAssertTrue(mock.last!.url!.query!.contains("days=31"), "days is clamped to the server's 1..31")
        XCTAssertEqual(usage.days.count, 2)
        XCTAssertEqual(usage.totals?.modelRequests, 8)
        XCTAssertEqual(usage.monthToDateUSD, 1.5)
        XCTAssertEqual(usage.budgetUSD, 3)
        XCTAssertEqual(usage.budgetFraction, 0.5)
        XCTAssertFalse(usage.isOverBudget)
        XCTAssertNotNil(usage.days[0].date)

        mock.on("GET", "/v1/usage/daily", status: 403, Data(#"{"title":"Usage is for workspace admins","status":403}"#.utf8))
        do { _ = try await client.usage(); XCTFail() } catch let APIError.forbidden(message) {
            XCTAssertEqual(message, "Usage is for workspace admins")
        }
    }

    func testModels() async throws {
        mock.on("GET", "/v1/models", try apiFixture("models"))
        let models = try await client.models()
        XCTAssertEqual(models.map(\.role), ["main", "triage"])
        XCTAssertEqual(models[1].missing, ["tool calling"])
        XCTAssertEqual(models[1].contextSize, "32k")

        mock.on("PUT", "/v1/models/main", try JSONSerialization.data(withJSONObject: (try JSONSerialization.jsonObject(with: apiFixture("models")) as! [Any])[0]))
        let request = ModelConnectRequest(provider: "anthropic", apiKey: "sk-x", model: "claude", baseUrl: nil,
                                          variables: [:], wire: "openai", auth: "bearer")
        XCTAssertTrue(request.validationErrors.isEmpty)
        _ = try await client.connectModel(role: .main, request)
        let body = try mock.lastJSON()
        XCTAssertEqual(body["provider"] as? String, "anthropic")
        XCTAssertEqual(body["api_key"] as? String, "sk-x")
        XCTAssertEqual(body["fallback_to_managed"] as? Bool, false)
        XCTAssertNil(body["wire"], "wire and auth are for custom endpoints only")
        XCTAssertNil(body["variables"])

        mock.on("DELETE", "/v1/models/triage", status: 204, Data())
        try await client.disconnectModel(role: .triage)
        XCTAssertEqual(mock.last?.httpMethod, "DELETE")
    }

    func testCustomModelRequestRules() {
        let custom = ModelConnectRequest(provider: "custom", apiKey: "k", model: "m", baseUrl: "https://x/v1//",
                                         variables: ["A": "b"], wire: "openai", auth: "bearer")
        XCTAssertEqual(custom.baseUrl, "https://x/v1")
        XCTAssertNil(custom.variables, "variables only travel without a base URL")
        XCTAssertEqual(custom.wire, "openai")
        let incomplete = ModelConnectRequest(provider: "custom", apiKey: " ", model: "", baseUrl: nil)
        XCTAssertEqual(incomplete.validationErrors.count, 5)
        let catalog = ModelConnectRequest(provider: "zeta", apiKey: "k", model: "m", variables: ["ENDPOINT_ID": "ep-1"])
        XCTAssertEqual(catalog.variables, ["ENDPOINT_ID": "ep-1"])
    }

    func testProviderCatalog() async throws {
        mock.on("GET", "/assets/providers.json", try apiFixture("providers"))
        let catalog = try await client.providerCatalog()
        XCTAssertEqual(catalog.all.map(\.id), ["anthropic", "alpha", "zeta"])
        XCTAssertEqual(catalog.all[2].variables?.first?.name, "ENDPOINT_ID")
        XCTAssertNil(mock.last?.value(forHTTPHeaderField: "Authorization"), "the catalog is public")
    }

    func testMe() async throws {
        mock.on("GET", "/v1/me", Data(#"{"workspace":{"id":"ws_1","name":"Acme"},"principal":{"kind":"member","id":"usr_1","name":"Ada","role":"owner","is_admin":true},"credential":{"kind":"personal_access_token","prefix":"lbp_0123456789abcdef","scopes":null}}"#.utf8))
        let me = try await client.me()
        XCTAssertEqual(me.workspaceLabel, "Acme (ws_1)")
        XCTAssertEqual(me.principalLabel, "Ada · owner")
        XCTAssertEqual(me.credential.prefix, "lbp_0123456789abcdef")
        XCTAssertNotNil(mock.last?.value(forHTTPHeaderField: "Authorization"))

        mock.on("GET", "/v1/me", Data(#"{"workspace":{"id":"ws_1","name":""},"principal":{"kind":"agent","id":"agt_1","name":"CI bot","on_behalf_of":{"id":"usr_1","name":"Ada"}},"credential":{"kind":"agent_key","prefix":"lba_00","scopes":["read:shared"]}}"#.utf8))
        let agent = try await client.me()
        XCTAssertEqual(agent.principalLabel, "CI bot (agent for Ada)")
        XCTAssertEqual(agent.workspaceLabel, "ws_1")
    }

    func testTokens() async throws {
        mock.on("GET", "/v1/tokens", try apiFixture("tokens"))
        let tokens = try await client.tokens()
        XCTAssertEqual(tokens.map(\.scopeLabel), ["Full access", "shared"])
        mock.on("DELETE", "/v1/tokens/lbp_0123456789abcdef", status: 204, Data())
        try await client.revokeToken(prefix: "lbp_0123456789abcdef")
        XCTAssertEqual(mock.last?.url?.path, "/v1/tokens/lbp_0123456789abcdef")
    }

    func testDeviceFlow() async throws {
        mock.on("POST", "/v1/device-auth/code", Data(#"{"device_code":"dc","user_code":"ABCD-EFGH","verification_uri":"https://app.livingbrain.wiki/device","verification_uri_complete":"https://app.livingbrain.wiki/device?user_code=ABCD-EFGH","interval":2,"expires_in":600}"#.utf8))
        let authorization = try await client.deviceAuthorize()
        XCTAssertEqual(authorization.userCode, "ABCD-EFGH")
        XCTAssertNil(mock.last?.value(forHTTPHeaderField: "Authorization"))
        XCTAssertEqual(try mock.lastJSON()["client_id"] as? String, "livingbrain-cli")
        XCTAssertEqual(APIClient.pollInterval(authorization.interval), 5)

        mock.on("POST", "/v1/device-auth/token", status: 400, Data(#"{"error":"authorization_pending"}"#.utf8))
        let pending = try await client.devicePoll(deviceCode: "dc")
        XCTAssertEqual(pending, .pending)
        let form = String(decoding: mock.last!.httpBody!, as: UTF8.self)
        XCTAssertEqual(form, "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=dc&client_id=livingbrain-cli")

        mock.on("POST", "/v1/device-auth/token", status: 400, Data(#"{"error":"slow_down"}"#.utf8))
        let slow = try await client.devicePoll(deviceCode: "dc")
        XCTAssertEqual(slow, .slowDown)
        mock.on("POST", "/v1/device-auth/token", Data(#"{"access_token":"lbp_1_secret","token_type":"bearer"}"#.utf8))
        let issued = try await client.devicePoll(deviceCode: "dc")
        XCTAssertEqual(issued, .issued("lbp_1_secret"))
        XCTAssertFalse(issued.description.contains("secret"))
        mock.on("POST", "/v1/device-auth/token", status: 400, Data(#"{"error":"expired_token"}"#.utf8))
        do { _ = try await client.devicePoll(deviceCode: "dc"); XCTFail() } catch {}
    }

    func testBaseURLWithAPath() {
        let nested = APIClient(baseURL: URL(string: "https://example.com/lb/")!, token: "t", transport: mock)
        XCTAssertEqual(nested.url("/v1/ask").absoluteString, "https://example.com/lb/v1/ask")
    }

    func testKeychainAccountMatchesTheCLI() {
        XCTAssertEqual(KeychainTokenStore.account(for: URL(string: "https://app.livingbrain.wiki/")!), "https://app.livingbrain.wiki")
        let store = MemoryTokenStore()
        XCTAssertNil(try store.read())
        try? store.write("t")
        XCTAssertEqual(try store.read(), "t")
        try? store.delete()
        XCTAssertNil(try store.read())
    }
}

final class MarkdownTests: XCTestCase {
    func testFrontmatterIsSplitOff() {
        let (front, body) = Markdown.splitFrontmatter("---\ntitle: X\n---\n\n# X\n")
        XCTAssertEqual(front, "title: X")
        XCTAssertEqual(body, "# X")
        XCTAssertNil(Markdown.splitFrontmatter("# no front").frontmatter)
    }

    func testBlocks() {
        let blocks = Markdown.blocks("""
        ---
        title: T
        ---
        # Title

        First line
        second line

        - a
        - b
          continued
        1. one
        2. two

        > quoted
        > more

        ```swift
        let x = 1
        ```
        ---
        """)
        XCTAssertEqual(blocks, [
            .heading(level: 1, text: "Title"),
            .paragraph("First line\nsecond line"),
            .bullets(["a", "b continued"]),
            .numbered(["one", "two"]),
            .quote("quoted\nmore"),
            .code(language: "swift", text: "let x = 1"),
            .rule,
        ])
    }

    func testWikilinks() {
        XCTAssertEqual(Markdown.linkWikilinks("see [[kestrel]] and [[q3-plan|the plan]]"),
                       "see [kestrel](livingbrain://page/kestrel) and [the plan](livingbrain://page/q3-plan)")
        XCTAssertEqual(Markdown.linkWikilinks("[[Not A Slug]]"), "[[Not A Slug]]")
    }
}

/// Counts `onWait` calls across the `@Sendable` boundary.
final class Waited: @unchecked Sendable {
    private let lock = NSLock()
    private var calls = 0
    func mark() { lock.lock(); calls += 1; lock.unlock() }
    var count: Int { lock.lock(); defer { lock.unlock() }; return calls }
}
