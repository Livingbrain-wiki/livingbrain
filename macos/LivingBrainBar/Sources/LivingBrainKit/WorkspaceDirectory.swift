import Foundation

/// A workspace this Mac holds a sign-in for. Only the label lives in user
/// defaults; the token is a keychain item of its own.
public struct SavedWorkspace: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    public var name: String

    public init(id: String, name: String) {
        self.id = id
        self.name = name
    }
}

/// Every workspace signed in on one server, and which one is in use.
///
/// A token belongs to exactly one workspace, so more workspaces means more
/// tokens: each is kept under the keychain account `<base URL>#<workspace
/// id>`. The single item older builds wrote (account `<base URL>`, no
/// workspace) is still read as the *unlabelled* sign-in until the server
/// says which workspace it is, and then moved under its id.
public final class WorkspaceDirectory {
    public let baseURL: URL
    private let defaults: UserDefaults
    private let tokenStore: (String) -> TokenStore

    public init(baseURL: URL, defaults: UserDefaults = .standard, tokenStore: @escaping (String) -> TokenStore) {
        self.baseURL = baseURL
        self.defaults = defaults
        self.tokenStore = tokenStore
    }

    private var base: String { KeychainTokenStore.account(for: baseURL) }
    private var listKey: String { "workspaces.\(base)" }
    private var activeKey: String { "workspaces.active.\(base)" }

    /// The keychain account a workspace's token is kept under.
    public static func account(baseURL: URL, workspaceId: String) -> String {
        "\(KeychainTokenStore.account(for: baseURL))#\(workspaceId)"
    }

    private func store(for id: String) -> TokenStore {
        tokenStore(Self.account(baseURL: baseURL, workspaceId: id))
    }

    private var legacy: TokenStore { tokenStore(base) }

    /// The signed-in workspaces, in the order they were added.
    public private(set) var workspaces: [SavedWorkspace] {
        get {
            guard let data = defaults.data(forKey: listKey) else { return [] }
            return (try? JSONDecoder().decode([SavedWorkspace].self, from: data)) ?? []
        }
        set { defaults.set(try? JSONEncoder().encode(newValue), forKey: listKey) }
    }

    /// The workspace in use, or nil while only the unlabelled sign-in (or
    /// none) is held.
    public private(set) var activeId: String? {
        get { defaults.string(forKey: activeKey) }
        set { defaults.set(newValue, forKey: activeKey) }
    }

    /// The token to use now: the active workspace's, else the unlabelled one.
    public func activeToken() -> String? {
        if let id = activeId, let token = try? store(for: id).read() { return token }
        return try? legacy.read()
    }

    /// Whether the token in use is the unlabelled one.
    public var activeIsUnlabelled: Bool {
        guard let id = activeId else { return true }
        return (try? store(for: id).read()) == nil
    }

    /// Keep `token` for `workspace` and make it the one in use. Signing in
    /// to a workspace already here replaces its token.
    public func save(token: String, for workspace: SavedWorkspace) throws {
        try store(for: workspace.id).write(token)
        var list = workspaces
        if let index = list.firstIndex(where: { $0.id == workspace.id }) {
            list[index] = workspace
        } else {
            list.append(workspace)
        }
        workspaces = list
        activeId = workspace.id
    }

    /// A token whose workspace is not known yet: kept unlabelled and used
    /// until [`label`] files it.
    public func saveUnlabelled(token: String) throws {
        try legacy.write(token)
        activeId = nil
    }

    /// The server named the unlabelled token's workspace: file it there.
    public func label(_ workspace: SavedWorkspace) throws {
        guard activeIsUnlabelled, let token = try legacy.read() else { return }
        try save(token: token, for: workspace)
        try legacy.delete()
    }

    /// The name changed on the server; keep the label current.
    public func rename(_ workspace: SavedWorkspace) {
        var list = workspaces
        guard let index = list.firstIndex(where: { $0.id == workspace.id }), list[index] != workspace else { return }
        list[index] = workspace
        workspaces = list
    }

    /// Use another signed-in workspace. False when it has no token here.
    @discardableResult
    public func activate(_ id: String) -> Bool {
        guard (try? store(for: id).read()) != nil else { return false }
        activeId = id
        return true
    }

    /// Forget the token in use and move to the next workspace, if any.
    /// Answers the workspace now in use.
    @discardableResult
    public func removeActive() -> String? {
        if let id = activeId, (try? store(for: id).read()) != nil {
            return remove(id)
        }
        try? legacy.delete()
        if let next = workspaces.first(where: { activate($0.id) }) { return next.id }
        activeId = nil
        return nil
    }

    /// Forget one workspace's token. Answers the workspace now in use.
    @discardableResult
    public func remove(_ id: String) -> String? {
        try? store(for: id).delete()
        workspaces = workspaces.filter { $0.id != id }
        if activeId == id {
            activeId = nil
            if let next = workspaces.first(where: { activate($0.id) }) { return next.id }
            return nil
        }
        return activeId
    }
}
