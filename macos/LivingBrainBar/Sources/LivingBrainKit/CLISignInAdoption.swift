import Foundation

/// Keeps the CLI's own sign-in as an app-owned item, and makes it the default
/// unless the user already chose a default.
///
/// The CLI's item is read by the app (see `WorkspaceModel`), and the token is
/// then stored under its workspace like any other sign-in. The CLI never reads
/// that item itself: the app hands the default token over in the environment
/// (see `CLITokenHandoff`).
public enum CLISignInAdoption {
    /// What adopting a CLI sign-in did.
    public struct Adoption: Equatable, Sendable {
        public let workspace: SavedWorkspace
        /// True when this sign-in became the default (so the app must record
        /// that in the opt-in file too).
        public let becameDefault: Bool
    }

    /// Store `token` under the workspace `whoAmI` names. It becomes the default
    /// only when no default is chosen yet; a user-chosen default stays. Nil when
    /// the server does not say which workspace the token speaks for: nothing is
    /// stored or changed then. Errors from `whoAmI` propagate, so the caller can
    /// retry later.
    public static func adopt(
        token: String,
        into directory: WorkspaceDirectory,
        activate: Bool,
        whoAmI: (String) async throws -> SavedWorkspace?
    ) async throws -> Adoption? {
        guard let workspace = try await whoAmI(token) else { return nil }
        try directory.save(token: token, for: workspace, activate: activate)
        let becomes = directory.defaultId == nil || !directory.defaultWasChosen
        if becomes {
            directory.defaultId = workspace.id
        }
        return Adoption(workspace: workspace, becameDefault: becomes)
    }
}

extension CLITokenHandoff {
    /// What the app hands its CLI: the default workspace's token and every
    /// signed-in workspace's token, all read from the app's own items.
    public init(directory: WorkspaceDirectory) {
        self.init(
            defaultToken: directory.defaultToken(),
            defaultWorkspaceId: directory.defaultId,
            workspaces: directory.signedInTokens(),
            apiURL: KeychainTokenStore.account(for: directory.baseURL)
        )
    }

    /// What the Mac app's job runner is handed: the default token and nothing
    /// else. The runner never needs a routed workspace's token.
    public init(defaultOnly directory: WorkspaceDirectory) {
        self.init(
            defaultToken: directory.defaultToken(),
            defaultWorkspaceId: directory.defaultId,
            workspaces: [],
            apiURL: KeychainTokenStore.account(for: directory.baseURL)
        )
    }
}
