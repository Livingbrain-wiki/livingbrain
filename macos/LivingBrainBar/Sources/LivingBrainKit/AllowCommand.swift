import Foundation

/// The `livingbrain logs allow` and `logs deny` commands the app runs.
///
/// Every allow names its workspace. A bare allow would send the repository's
/// sessions to the CLI's default token, which on a machine signed in to several
/// workspaces is usually the wrong one; the CLI refuses it too (see `logs
/// allow`), but the app never asks for it.
public enum AllowCommand {
    public enum Failure: Error, Equatable, LocalizedError {
        case invalidWorkspace(String)

        public var errorDescription: String? {
            switch self {
            case let .invalidWorkspace(id):
                return "`\(id)` is not a workspace id this app can use."
            }
        }
    }

    /// The workspace a new opt-in goes to without asking: the one in use, else
    /// the only one signed in. Nil when the user must choose (several signed in
    /// and none in use) or can choose nothing (none signed in).
    public static func defaultWorkspace(active: String?, signedIn: [SavedWorkspace]) -> String? {
        if let active, !active.isEmpty { return active }
        return signedIn.count == 1 ? signedIn[0].id : nil
    }

    /// Arguments for `logs allow <path> --workspace <id>`, with `--detail` when
    /// given. Throws for an id the CLI would refuse, so no argument list ever
    /// reaches the CLI without a valid workspace.
    public static func allowArguments(path: String, workspace: String, detail: String? = nil) throws -> [String] {
        guard isValidWorkspaceID(workspace) else { throw Failure.invalidWorkspace(workspace) }
        var arguments = ["--json", "logs", "allow", path, "--workspace", workspace]
        if let detail, !detail.isEmpty { arguments += ["--detail", detail] }
        return arguments
    }

    /// Arguments for `logs deny <path>`: removing consent needs no workspace.
    public static func denyArguments(path: String) -> [String] {
        ["--json", "logs", "deny", path]
    }

    /// Mirrors the CLI's `check_target`: letters, digits, `-`, `_` and `.`, at
    /// most 128 bytes, and not the reserved word `auto`.
    static func isValidWorkspaceID(_ id: String) -> Bool {
        !id.isEmpty
            && id != "auto"
            && id.utf8.count <= 128
            && id.utf8.allSatisfy { byte in
                (byte >= 0x30 && byte <= 0x39) || (byte >= 0x41 && byte <= 0x5A)
                    || (byte >= 0x61 && byte <= 0x7A) || byte == 0x2D || byte == 0x5F || byte == 0x2E
            }
    }
}
