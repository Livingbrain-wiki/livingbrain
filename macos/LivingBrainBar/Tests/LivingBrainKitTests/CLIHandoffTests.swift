import Foundation
@testable import LivingBrainKit
import XCTest

extension XCTestCase {
    /// A user-defaults suite that is removed when the test ends, so no test
    /// leaves state behind in the developer's defaults.
    func temporaryDefaults(_ tag: String) -> UserDefaults {
        let name = "\(tag).\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: name)!
        addTeardownBlock {
            defaults.removePersistentDomain(forName: name)
        }
        return defaults
    }
}

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
        // With a handoff the server is the tokens', never the inherited one.
        XCTAssertEqual(env["LIVINGBRAIN_API_URL"], CLIEnvironment.defaultAPIURL)
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

    /// The default workspace's token is `LIVINGBRAIN_TOKEN` and the id it must
    /// belong to rides with it. Without a token, neither is set.
    func testTheDefaultTokenAndItsWorkspaceIdAreHanded() {
        let withDefault = CLITokenHandoff(defaultToken: "tok-default", defaultWorkspaceId: "ws_earth")
        let env = CLIEnvironment.make(base: [:], handoff: withDefault)
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN"], "tok-default")
        XCTAssertEqual(env["LIVINGBRAIN_DEFAULT_WORKSPACE"], "ws_earth")

        let noToken = CLITokenHandoff(defaultToken: nil, defaultWorkspaceId: "ws_earth")
        let bare = CLIEnvironment.make(base: [:], handoff: noToken)
        XCTAssertNil(bare["LIVINGBRAIN_TOKEN"])
        XCTAssertNil(bare["LIVINGBRAIN_DEFAULT_WORKSPACE"])
    }

    /// Every CLI run talks to the server the tokens came from, even when the
    /// launch environment names another.
    func testTheAPIURLOfTheTokensOverridesAnInheritedOne() {
        let handoff = CLITokenHandoff(defaultToken: "tok", apiURL: "https://app.livingbrain.wiki")
        let env = CLIEnvironment.make(base: ["LIVINGBRAIN_API_URL": "https://evil.example"], handoff: handoff)
        XCTAssertEqual(env["LIVINGBRAIN_API_URL"], "https://app.livingbrain.wiki")
    }

    /// The job runner is handed the default token only: no routed workspace's.
    func testTheRunnerIsHandedTheDefaultTokenAndNoWorkspaceTokens() throws {
        let defaults = temporaryDefaults("RunnerHandoffTests")
        let base = URL(string: "https://app.livingbrain.wiki/")!
        var items: [String: MemoryTokenStore] = [:]
        let dir = WorkspaceDirectory(baseURL: base, defaults: defaults) { account in
            if let store = items[account] { return store }
            let store = MemoryTokenStore()
            items[account] = store
            return store
        }
        try dir.save(token: "tok-chi", for: SavedWorkspace(id: "ws_chi", name: "CHI"))
        try dir.save(token: "tok-earth", for: SavedWorkspace(id: "ws_earth", name: "EarthOS"), activate: false)
        dir.defaultId = "ws_earth"

        let env = CLIEnvironment.make(base: [:], handoff: CLITokenHandoff(defaultOnly: dir))
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN"], "tok-earth")
        XCTAssertEqual(env["LIVINGBRAIN_DEFAULT_WORKSPACE"], "ws_earth")
        XCTAssertFalse(env.keys.contains("LIVINGBRAIN_TOKEN_WS_CHI"))
        XCTAssertFalse(env.values.contains("tok-chi"))
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

    /// Auto-pick only with exactly one signed-in workspace. Two signed in and one
    /// in use still needs the user's choice: the one in use is never assumed.
    func testTwoSignedInWithOneActiveStillRequiresAPrompt() {
        let chi = SavedWorkspace(id: "ws_chi", name: "CHI")
        let earth = SavedWorkspace(id: "ws_earth", name: "EarthOS")
        XCTAssertNil(AllowCommand.defaultWorkspace(signedIn: [chi, earth]), "the user must choose")
        XCTAssertEqual(AllowCommand.defaultWorkspace(signedIn: [chi]), "ws_chi")
        XCTAssertNil(AllowCommand.defaultWorkspace(signedIn: []), "nothing may be opted in")
    }

    /// The chooser offers the workspace in use first, so it is the default
    /// button and nothing else is preselected.
    func testTheChooserPutsTheWorkspaceInUseFirst() {
        let chi = SavedWorkspace(id: "ws_chi", name: "CHI")
        let earth = SavedWorkspace(id: "ws_earth", name: "EarthOS")
        XCTAssertEqual(AllowCommand.pickerOrder(active: "ws_earth", signedIn: [chi, earth]).map(\.id), ["ws_earth", "ws_chi"])
        XCTAssertEqual(AllowCommand.pickerOrder(active: nil, signedIn: [chi, earth]).map(\.id), ["ws_chi", "ws_earth"])
        XCTAssertEqual(AllowCommand.pickerOrder(active: "gone", signedIn: [chi, earth]).map(\.id), ["ws_chi", "ws_earth"])
    }
}

final class SignedInTokensTests: XCTestCase {
    func testOnlyWorkspacesWithATokenAreHandedOver() throws {
        let base = URL(string: "https://app.livingbrain.wiki/")!
        let defaults = temporaryDefaults("SignedInTokensTests")
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

final class DefaultWorkspaceHandoffTests: XCTestCase {
    private let base = URL(string: "https://app.livingbrain.wiki/")!
    private var defaults: UserDefaults!
    private var items: [String: MemoryTokenStore] = [:]

    override func setUp() {
        defaults = temporaryDefaults("DefaultWorkspaceHandoffTests")
        items = [:]
    }

    private func directory() -> WorkspaceDirectory {
        WorkspaceDirectory(baseURL: base, defaults: defaults) { account in
            if let store = self.items[account] { return store }
            let store = MemoryTokenStore()
            self.items[account] = store
            return store
        }
    }

    func testTheDefaultWorkspacesTokenIsTheCLIsLoginToken() throws {
        let dir = directory()
        try dir.save(token: "tok-earth-DEFAULT", for: SavedWorkspace(id: "ws_earth", name: "EarthOS"), activate: false)
        try dir.save(token: "tok-chi", for: SavedWorkspace(id: "ws_chi", name: "CHI"))
        dir.defaultId = "ws_earth"

        let env = CLIEnvironment.make(base: [:], handoff: CLITokenHandoff(directory: dir))
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN"], "tok-earth-DEFAULT")
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN_WS_CHI"], "tok-chi")
        XCTAssertEqual(dir.activeId, "ws_chi", "adopting a default never switches the workspace in use")
    }

    func testWithoutADefaultTheCLIGetsNoDefaultToken() throws {
        let dir = directory()
        try dir.save(token: "tok-chi", for: SavedWorkspace(id: "ws_chi", name: "CHI"))
        let env = CLIEnvironment.make(base: [:], handoff: CLITokenHandoff(directory: dir))
        XCTAssertNil(env["LIVINGBRAIN_TOKEN"], "the CLI then reports NotLoggedIn for the default destination")
        XCTAssertEqual(env["LIVINGBRAIN_TOKEN_WS_CHI"], "tok-chi", "routed groups still work")
    }

    func testADefaultWhoseSignInIsGoneGetsNoDefaultToken() throws {
        let dir = directory()
        try dir.save(token: "tok-earth", for: SavedWorkspace(id: "ws_earth", name: "EarthOS"))
        dir.defaultId = "ws_earth"
        try items[WorkspaceDirectory.account(baseURL: base, workspaceId: "ws_earth")]?.delete()
        XCTAssertNil(CLITokenHandoff(directory: dir).variables["LIVINGBRAIN_TOKEN"])
    }

    func testRemovingTheDefaultWorkspaceClearsTheDefault() throws {
        let dir = directory()
        try dir.save(token: "tok-earth", for: SavedWorkspace(id: "ws_earth", name: "EarthOS"))
        dir.defaultId = "ws_earth"
        dir.remove("ws_earth")
        XCTAssertNil(dir.defaultId)
    }
}

final class CLISignInAdoptionTests: XCTestCase {
    private let base = URL(string: "https://app.livingbrain.wiki/")!

    private func directory(_ defaults: UserDefaults, _ items: @escaping (String) -> TokenStore) -> WorkspaceDirectory {
        WorkspaceDirectory(baseURL: base, defaults: defaults, tokenStore: items)
    }

    private func storeMap() -> (WorkspaceDirectory, UserDefaults) {
        let defaults = temporaryDefaults("CLISignInAdoptionTests")
        var items: [String: MemoryTokenStore] = [:]
        let dir = directory(defaults) { account in
            if let store = items[account] { return store }
            let store = MemoryTokenStore()
            items[account] = store
            return store
        }
        return (dir, defaults)
    }

    func testTheCLISignInBecomesAnAppOwnedItemAndTheDefaultWhenNoneIsChosen() async throws {
        let (dir, _) = storeMap()
        try dir.save(token: "tok-chi", for: SavedWorkspace(id: "ws_chi", name: "CHI"))

        let adoption = try await CLISignInAdoption.adopt(token: "tok-cli", into: dir, activate: false) { token in
            XCTAssertEqual(token, "tok-cli")
            return SavedWorkspace(id: "ws_earth", name: "EarthOS")
        }

        XCTAssertEqual(adoption?.workspace, SavedWorkspace(id: "ws_earth", name: "EarthOS"))
        XCTAssertEqual(adoption?.becameDefault, true)
        XCTAssertEqual(dir.defaultId, "ws_earth")
        XCTAssertEqual(dir.defaultToken(), "tok-cli")
        XCTAssertEqual(dir.activeId, "ws_chi", "activation is left as it was")
    }

    /// A CLI login run from the app must not override a default the user chose.
    func testAUserChosenDefaultSurvivesAnAdoption() async throws {
        let (dir, _) = storeMap()
        try dir.save(token: "tok-chi", for: SavedWorkspace(id: "ws_chi", name: "CHI"))
        dir.defaultId = "ws_chi"
        dir.defaultWasChosen = true

        let adoption = try await CLISignInAdoption.adopt(token: "tok-cli", into: dir, activate: false) { _ in
            SavedWorkspace(id: "ws_earth", name: "EarthOS")
        }

        XCTAssertEqual(adoption?.becameDefault, false)
        XCTAssertEqual(dir.defaultId, "ws_chi")
        XCTAssertEqual(dir.defaultToken(), "tok-chi")
        XCTAssertEqual(dir.workspaces.map(\.id).sorted(), ["ws_chi", "ws_earth"], "the sign-in is still kept")
    }

    func testAnUnnamedCLISignInChangesNothing() async throws {
        let (dir, _) = storeMap()
        let adoption = try await CLISignInAdoption.adopt(token: "tok-cli", into: dir, activate: true) { _ in nil }
        XCTAssertNil(adoption)
        XCTAssertNil(dir.defaultId)
        XCTAssertTrue(dir.workspaces.isEmpty)
    }

    /// A server failure propagates, so the caller retries later instead of
    /// marking the sign-in as checked.
    func testAServerFailurePropagates() async {
        let (dir, _) = storeMap()
        struct Down: Error {}
        do {
            _ = try await CLISignInAdoption.adopt(token: "tok", into: dir, activate: true) { _ in throw Down() }
            XCTFail("expected the failure to propagate")
        } catch {
            XCTAssertTrue(error is Down)
        }
        XCTAssertNil(dir.defaultId)
    }
}
