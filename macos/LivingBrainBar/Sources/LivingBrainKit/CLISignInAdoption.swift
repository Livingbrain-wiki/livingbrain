import Foundation

/// Keeps the CLI's own sign-in as an app-owned item and makes it the default.
///
/// The CLI's item is read by the app once (and again after a CLI `login` run
/// from the app), and the token is then stored under its workspace like any
/// other sign-in. The CLI never reads that item itself: the app hands the
/// default token over in the environment (see `CLITokenHandoff`).
public enum CLISignInAdoption {
    /// Store `token` under the workspace `whoAmI` names and make that workspace
    /// the default. Nil when the server does not say which workspace the token
    /// speaks for: nothing is stored or changed then.
    @discardableResult
    public static func adopt(
        token: String,
        into directory: WorkspaceDirectory,
        activate: Bool,
        whoAmI: (String) async throws -> SavedWorkspace?
    ) async throws -> SavedWorkspace? {
        guard let workspace = try await whoAmI(token) else { return nil }
        try directory.save(token: token, for: workspace, activate: activate)
        directory.defaultId = workspace.id
        return workspace
    }
}

extension CLITokenHandoff {
    /// What the app hands its CLI: the default workspace's token and every
    /// signed-in workspace's token, all read from the app's own items.
    public init(directory: WorkspaceDirectory) {
        self.init(defaultToken: directory.defaultToken(), workspaces: directory.signedInTokens())
    }
}
