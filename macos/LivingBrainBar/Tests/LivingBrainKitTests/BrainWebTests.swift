import Foundation
@testable import LivingBrainKit
import XCTest

final class BrainWebTests: XCTestCase {
    private let server = URL(string: "https://app.livingbrain.wiki")!

    func testEntryURLIsTheBrainPageOnTheBrainScheme() {
        XCTAssertEqual(BrainWeb.entryURL.absoluteString, "lbbrain://livingbrain/brain")
    }

    func testOnlyReadsUnderV1CarryTheCredential() {
        XCTAssertEqual(BrainWeb.upstream(method: "GET", path: "/v1/graph"), .bearer)
        XCTAssertEqual(BrainWeb.upstream(method: "HEAD", path: "/v1/graph"), .bearer)
        XCTAssertEqual(BrainWeb.upstream(method: "GET", path: "/v1"), .bearer)
        XCTAssertEqual(BrainWeb.upstream(method: "GET", path: "/brain"), .anonymous)
        XCTAssertEqual(BrainWeb.upstream(method: "GET", path: "/assets/app.js"), .anonymous)
        XCTAssertEqual(BrainWeb.upstream(method: "GET", path: "/v1x/graph"), .anonymous)
    }

    func testWritesAreRefusedEverywhere() {
        XCTAssertEqual(BrainWeb.upstream(method: "POST", path: "/v1/workspaces/signout"), .refused)
        XCTAssertEqual(BrainWeb.upstream(method: "DELETE", path: "/v1/models/mine/main"), .refused)
        XCTAssertEqual(BrainWeb.upstream(method: "PUT", path: "/v1/pages/x"), .refused)
        XCTAssertEqual(BrainWeb.upstream(method: "POST", path: "/brain"), .refused)
    }

    func testServerURLKeepsPathAndQueryAndTheServerOrigin() {
        let url = URL(string: "lbbrain://livingbrain/v1/graph?limit=1&project=a%20b")!
        XCTAssertEqual(
            BrainWeb.serverURL(for: url, server: server)?.absoluteString,
            "https://app.livingbrain.wiki/v1/graph?limit=1&project=a%20b"
        )
    }

    func testServerURLKeepsAServerPathPrefix() {
        let prefixed = URL(string: "https://example.test/lb/")!
        let url = URL(string: "lbbrain://livingbrain/v1/graph")!
        XCTAssertEqual(BrainWeb.serverURL(for: url, server: prefixed)?.absoluteString, "https://example.test/lb/v1/graph")
    }

    func testServerURLRejectsOtherSchemes() {
        XCTAssertNil(BrainWeb.serverURL(for: URL(string: "https://evil.test/v1/graph")!, server: server))
    }

    func testBrainPageNavigationStaysInTheView() {
        XCTAssertEqual(navigation("lbbrain://livingbrain/brain"), .allow)
        XCTAssertEqual(navigation("lbbrain://livingbrain/brain#node=3"), .allow)
        XCTAssertEqual(navigation("lbbrain://livingbrain/brain.html"), .allow)
        XCTAssertEqual(navigation("about:blank"), .allow)
    }

    func testOtherPagesOfTheAppOpenInTheBrowserOnTheServer() {
        XCTAssertEqual(
            navigation("lbbrain://livingbrain/wiki.html?slug=x"),
            .browser(URL(string: "https://app.livingbrain.wiki/wiki.html?slug=x")!)
        )
        XCTAssertEqual(navigation("lbbrain://other/brain"), .browser(URL(string: "https://app.livingbrain.wiki/brain")!))
    }

    func testExternalLinksOpenInTheBrowser() {
        XCTAssertEqual(navigation("https://github.com/Livingbrain-wiki"), .browser(URL(string: "https://github.com/Livingbrain-wiki")!))
        XCTAssertEqual(navigation("http://example.test/a"), .browser(URL(string: "http://example.test/a")!))
        XCTAssertEqual(navigation("mailto:a@example.test"), .browser(URL(string: "mailto:a@example.test")!))
    }

    func testOtherSchemesAreCancelled() {
        XCTAssertEqual(navigation("javascript:alert(1)"), .cancel)
        XCTAssertEqual(navigation("file:///etc/passwd"), .cancel)
        XCTAssertEqual(navigation("tel:+6512345678"), .cancel)
    }

    func testRedirectsFollowOnlyTheSameOrigin() {
        XCTAssertTrue(BrainWeb.sameOrigin(URL(string: "https://app.livingbrain.wiki/brain")!, URL(string: "https://APP.livingbrain.wiki:443/x")!))
        XCTAssertFalse(BrainWeb.sameOrigin(URL(string: "https://app.livingbrain.wiki/brain")!, URL(string: "https://login.example.test/")!))
        XCTAssertFalse(BrainWeb.sameOrigin(URL(string: "https://app.livingbrain.wiki/")!, URL(string: "http://app.livingbrain.wiki/")!))
    }

    func testWireEncodingAndCookieHeadersAreNotPassedOn() {
        XCTAssertFalse(BrainWeb.passesHeader("Set-Cookie"))
        XCTAssertFalse(BrainWeb.passesHeader("content-encoding"))
        XCTAssertFalse(BrainWeb.passesHeader("Content-Length"))
        XCTAssertTrue(BrainWeb.passesHeader("Content-Type"))
        XCTAssertTrue(BrainWeb.passesHeader("Content-Security-Policy"))
    }

    func testSessionCheckIsAnsweredFromTheBearerIdentity() {
        XCTAssertEqual(BrainWeb.upstreamPath(method: "GET", path: "/v1/workspaces/me"), "/v1/me")
        XCTAssertEqual(BrainWeb.upstreamPath(method: "GET", path: "/v1/graph"), "/v1/graph")
        XCTAssertEqual(BrainWeb.upstreamPath(method: "POST", path: "/v1/workspaces/me"), "/v1/workspaces/me")
    }

    func testSessionShimReshapesAMemberIdentity() throws {
        let me = Data("""
        {"workspace":{"id":"w1","name":"CHI","avatar_url":null},
         "principal":{"kind":"member","id":"u1","name":"Nick","display_name":null,"avatar_url":null,"role":"admin"},
         "credential":{"kind":"token"}}
        """.utf8)
        let shim = try XCTUnwrap(BrainWeb.sessionShim(fromMe: me))
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: shim) as? [String: Any])
        XCTAssertEqual((object["workspace"] as? [String: Any])?["name"] as? String, "CHI")
        XCTAssertEqual((object["member"] as? [String: Any])?["id"] as? String, "u1")
        XCTAssertEqual(object["is_owner"] as? Bool, false)
        XCTAssertNil(object["credential"])
    }

    func testSessionShimRefusesAnAgentKey() {
        let agent = Data(#"{"workspace":{"id":"w1","name":"CHI"},"principal":{"kind":"agent","id":"a1","name":"bot"}}"#.utf8)
        XCTAssertNil(BrainWeb.sessionShim(fromMe: agent))
        XCTAssertNil(BrainWeb.sessionShim(fromMe: Data("not json".utf8)))
    }

    private func navigation(_ string: String) -> BrainWeb.Navigation {
        BrainWeb.navigation(to: URL(string: string)!, server: server)
    }
}
