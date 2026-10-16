import Foundation
@testable import LivingBrainKit
import XCTest

final class WorkspaceDirectoryTests: XCTestCase {
    private let base = URL(string: "https://app.livingbrain.wiki/")!
    private var defaults: UserDefaults!
    private var items: [String: MemoryTokenStore] = [:]

    override func setUp() {
        defaults = UserDefaults(suiteName: "WorkspaceDirectoryTests.\(UUID().uuidString)")
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

    func testEachWorkspaceKeepsItsOwnTokenAndSwitchingPicksIt() throws {
        let dir = directory()
        try dir.save(token: "tok-a", for: SavedWorkspace(id: "ws_a", name: "Alpha"))
        try dir.save(token: "tok-b", for: SavedWorkspace(id: "ws_b", name: "Beta"))
        XCTAssertEqual(dir.workspaces.map(\.id), ["ws_a", "ws_b"])
        XCTAssertEqual(dir.activeToken(), "tok-b")
        XCTAssertTrue(dir.activate("ws_a"))
        XCTAssertEqual(dir.activeToken(), "tok-a")
        XCTAssertEqual(try items["https://app.livingbrain.wiki#ws_b"]?.read(), "tok-b")
        XCTAssertFalse(dir.activate("ws_unknown"))
        XCTAssertEqual(dir.activeId, "ws_a")
    }

    func testSigningInAgainReplacesTheTokenNotTheEntry() throws {
        let dir = directory()
        try dir.save(token: "old", for: SavedWorkspace(id: "ws_a", name: "Alpha"))
        try dir.save(token: "new", for: SavedWorkspace(id: "ws_a", name: "Alpha 2"))
        XCTAssertEqual(dir.workspaces, [SavedWorkspace(id: "ws_a", name: "Alpha 2")])
        XCTAssertEqual(dir.activeToken(), "new")
    }

    func testTheOlderSingleSignInIsUsedThenFiledUnderItsWorkspace() throws {
        items["https://app.livingbrain.wiki"] = MemoryTokenStore("legacy")
        let dir = directory()
        XCTAssertTrue(dir.activeIsUnlabelled)
        XCTAssertEqual(dir.activeToken(), "legacy")
        try dir.label(SavedWorkspace(id: "ws_e", name: "EarthOS"))
        XCTAssertFalse(dir.activeIsUnlabelled)
        XCTAssertEqual(dir.activeId, "ws_e")
        XCTAssertEqual(dir.activeToken(), "legacy")
        XCTAssertNil(try items["https://app.livingbrain.wiki"]?.read())
    }

    func testRemovingTheActiveWorkspaceMovesToTheNext() throws {
        let dir = directory()
        try dir.save(token: "tok-a", for: SavedWorkspace(id: "ws_a", name: "Alpha"))
        try dir.save(token: "tok-b", for: SavedWorkspace(id: "ws_b", name: "Beta"))
        XCTAssertEqual(dir.removeActive(), "ws_a")
        XCTAssertEqual(dir.activeToken(), "tok-a")
        XCTAssertEqual(dir.workspaces.map(\.id), ["ws_a"])
        XCTAssertNil(dir.remove("ws_a"))
        XCTAssertNil(dir.activeToken())
        XCTAssertTrue(dir.workspaces.isEmpty)
    }

    func testRemovingAnotherWorkspaceKeepsTheActiveOne() throws {
        let dir = directory()
        try dir.save(token: "tok-a", for: SavedWorkspace(id: "ws_a", name: "Alpha"))
        try dir.save(token: "tok-b", for: SavedWorkspace(id: "ws_b", name: "Beta"))
        XCTAssertEqual(dir.remove("ws_a"), "ws_b")
        XCTAssertEqual(dir.activeToken(), "tok-b")
    }
}
