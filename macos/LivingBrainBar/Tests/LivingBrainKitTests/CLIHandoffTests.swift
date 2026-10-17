import Foundation
@testable import LivingBrainKit
import XCTest

final class CLIHandoffTests: XCTestCase {
    private let secretChi = "tok-chi-SECRET-111"
    private let secretEarth = "tok-earth-SECRET-222"

    private func handoff() -> CLITokenHandoff {
        CLITokenHandoff(defaultToken: nil, workspaces: [
            CLIWorkspaceToken(id: "ws_chi", token: secretChi),
            CLIWorkspaceToken(id: "ws-earth.2", token: secretEarth),
        ])
    }

    func testEachWorkspaceTokenGoesInItsOwnVariableNamedAsTheCLIReadsIt() {
        XCTAssertEqual(CLIEnvironment.workspaceVariable("ws_chi"), "LIVINGBRAIN_TOKEN_WS_CHI")
        XCTAssertEqual(CLIEnvironment.workspaceVariable("ws-earth.2"), "LIVINGBRAIN_TOKEN_WS_EARTH_2")

        let env = CLIEnvironment.make(base: ["PATH": "/usr/bin"], handoff: handoff())
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN_WS_CHI"], secretChi)
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN_WS_EARTH_2"], secretEarth)
        XCTAssertNil(env["LIVINGBRAIN_TOKEN"], "the app does not hold the CLI's own sign-in")
    }

    func testTheKeychainIsSwitchedOffAndTheWorkspaceHintIsSetOnlyWithTokens() {
        let env = CLIEnvironment.make(base: [:], handoff: handoff())
        XCTAssertEqual(env["LIVINGBRAIN_NO_KEYCHAIN"], "1")
        XCTAssertEqual(env["LIVINGBRAIN_MULTI_WORKSPACE"], "1")

        let none = CLIEnvironment.make(base: [:], handoff: CLITokenHandoff())
        XCTAssertEqual(none["LIVINGBRAIN_NO_KEYCHAIN"], "1")
        XCTAssertNil(none["LIVINGBRAIN_MULTI_WORKSPACE"])
        XCTAssertFalse(none.keys.contains { $0.hasPrefix("LIVINGBRAIN_TOKEN") })
    }

    func testTokensInheritedFromTheLaunchEnvironmentNeverReachTheChild() {
        let base = [
            "PATH": "/usr/bin",
            "LIVINGBRAIN_TOKEN": "stale-default",
            "LIVINGBRAIN_TOKEN_WS_OLD": "stale-workspace",
            "LIVINGBRAIN_API_URL": "https://example.test",
        ]
        let env = CLIEnvironment.make(base: base, handoff: handoff())
        XCTAssertFalse(env.values.contains("stale-default"))
        XCTAssertFalse(env.values.contains("stale-workspace"))
        XCTAssertNil(env["LIVINGBRAIN_TOKEN_WS_OLD"])
        XCTAssertEqual(env["LIVINGBRAIN_API_URL"], "https://example.test")
        XCTAssertTrue(env["PATH"]?.contains("/usr/bin") == true)
    }

    func testIdsThatShareAVariableNameAreBothWithheld() {
        let handoff = CLITokenHandoff(workspaces: [
            CLIWorkspaceToken(id: "a-b", token: "tok-dash"),
            CLIWorkspaceToken(id: "a_b", token: "tok-underscore"),
            CLIWorkspaceToken(id: "ws_x", token: "tok-x"),
        ])
        let vars = handoff.variables
        XCTAssertNil(vars["LIVINGBRAIN_TOKEN_A_B"], "neither token may land in the other's variable")
        XCTAssertFalse(vars.values.contains("tok-dash"))
        XCTAssertFalse(vars.values.contains("tok-underscore"))
        XCTAssertEqual(vars["LIVINGBRAIN_TOKEN_WS_X"], "tok-x")
    }

    func testLoginKeepsTheEnvironmentAsItWasWithNoHandoff() {
        let env = CLIEnvironment.make(base: ["LIVINGBRAIN_TOKEN": "kept"])
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN"], "kept")
        XCTAssertNil(env["LIVINGBRAIN_NO_KEYCHAIN"], "login must keep the keychain to save the CLI's sign-in")
    }

    func testTokensAreNeverPrintedByTheHandoff() {
        let handoff = handoff()
        for text in [handoff.description, handoff.debugDescription, String(describing: handoff), String(reflecting: handoff)] {
            XCTAssertFalse(text.contains(secretChi), text)
            XCTAssertFalse(text.contains(secretEarth), text)
        }
    }
}

final class AllowCommandTests: XCTestCase {
    func testAnAllowAlwaysNamesItsWorkspace() throws {
        let arguments = try AllowCommand.allowArguments(path: "/Users/a/repo", workspace: "ws_chi")
        XCTAssertEqual(arguments, ["--json", "logs", "allow", "/Users/a/repo", "--workspace", "ws_chi"])
        XCTAssertTrue(arguments.contains("--workspace"))
    }

    func testTheDetailLevelIsAppendedWhenGiven() throws {
        let arguments = try AllowCommand.allowArguments(path: "/r", workspace: "default", detail: "messages")
        XCTAssertEqual(arguments.suffix(4), ["--workspace", "default", "--detail", "messages"])
    }

    func testAnUnusableWorkspaceIsRefusedNotDropped() {
        for id in ["", "auto", "bad id", "ws/chi", String(repeating: "a", count: 129)] {
            XCTAssertThrowsError(try AllowCommand.allowArguments(path: "/r", workspace: id), id)
        }
    }

    func testDenyNeedsNoWorkspace() {
        XCTAssertEqual(AllowCommand.denyArguments(path: "/r"), ["--json", "logs", "deny", "/r"])
    }

    func testTheWorkspaceIsTheOneInUseElseTheOnlyOneElseNothing() {
        let chi = SavedWorkspace(id: "ws_chi", name: "CHI")
        let earth = SavedWorkspace(id: "ws_earth", name: "EarthOS")
        XCTAssertEqual(AllowCommand.defaultWorkspace(active: "ws_chi", signedIn: [chi, earth]), "ws_chi")
        XCTAssertEqual(AllowCommand.defaultWorkspace(active: nil, signedIn: [chi]), "ws_chi")
        XCTAssertNil(AllowCommand.defaultWorkspace(active: nil, signedIn: [chi, earth]), "the user must choose")
        XCTAssertNil(AllowCommand.defaultWorkspace(active: nil, signedIn: []), "nothing may be opted in")
    }
}

final class SignedInTokensTests: XCTestCase {
    func testOnlyWorkspacesWithATokenAreHandedOver() throws {
        let base = URL(string: "https://app.livingbrain.wiki/")!
        let defaults = UserDefaults(suiteName: "SignedInTokensTests.\(UUID().uuidString)")!
        var items: [String: MemoryTokenStore] = [:]
        let directory = WorkspaceDirectory(baseURL: base, defaults: defaults) { account in
            if let store = items[account] { return store }
            let store = MemoryTokenStore()
            items[account] = store
            return store
        }
        try directory.save(token: "tok-a", for: SavedWorkspace(id: "ws_a", name: "A"))
        try directory.save(token: "tok-b", for: SavedWorkspace(id: "ws_b", name: "B"))
        try items[WorkspaceDirectory.account(baseURL: base, workspaceId: "ws_b")]?.delete()

        XCTAssertEqual(directory.signedInTokens(), [CLIWorkspaceToken(id: "ws_a", token: "tok-a")])
    }
}
