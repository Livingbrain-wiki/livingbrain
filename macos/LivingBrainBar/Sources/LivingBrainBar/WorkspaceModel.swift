import AppKit
import Foundation
import LivingBrainKit

/// The signed-in side of the app: one API token per workspace, the one in
/// use, and a client built on it. Tokens live in the keychain and in this
/// object's memory, and only ever travel in the `Authorization` header.
@MainActor
final class WorkspaceModel: ObservableObject {
    enum SignIn: Equatable {
        case idle
        case waiting(code: String, url: URL?)
        case failed(String)
    }

    @Published private(set) var signedIn = false
    @Published private(set) var signIn: SignIn = .idle
    /// Set when the server refused the stored token.
    @Published private(set) var authProblem: String?
    /// From `/v1/usage/daily` when the caller is an admin: the only
    /// workspace identity a bearer token can read (`/v1/workspaces/me` is
    /// cookie-only).
    @Published var workspaceId: String?
    /// From `GET /v1/me`: who the saved token is, and in which workspace.
    @Published private(set) var me: Me?
    /// Every workspace signed in on this server, in the order added.
    @Published private(set) var workspaces: [SavedWorkspace] = []
    /// The workspace in use; nil while the token's workspace is not known.
    /// Views key their state on it so switching starts them afresh.
    @Published private(set) var activeId: String?

    let bar: BarModel
    private var token: String?
    private var signInTask: Task<Void, Never>?

    /// `--api-fixtures <dir>`: answer every call from recorded JSON (see
    /// `FixtureTransport`) — for UI checks without a server or a token.
    private let fixtures: URL? = {
        let args = CommandLine.arguments
        guard let i = args.firstIndex(of: "--api-fixtures"), i + 1 < args.count else { return nil }
        return URL(fileURLWithPath: args[i + 1])
    }()

    init(bar: BarModel) {
        self.bar = bar
        if fixtures != nil {
            token = "fixture"
            signedIn = true
        } else {
            reloadDirectory()
        }
    }

    var baseURL: URL { bar.serverURL }

    private var directory: WorkspaceDirectory {
        WorkspaceDirectory(baseURL: baseURL) {
            KeychainTokenStore(service: KeychainTokenStore.appService, account: $0)
        }
    }

    /// Read the saved workspaces and the token in use for this server.
    private func reloadDirectory() {
        let directory = self.directory
        workspaces = directory.workspaces
        activeId = directory.activeIsUnlabelled ? nil : directory.activeId
        token = directory.activeToken()
        signedIn = token != nil
    }

    /// The saved name of the workspace in use, before `/v1/me` answers.
    var activeName: String? {
        me?.workspace.name ?? workspaces.first { $0.id == activeId }?.name
    }

    var client: APIClient {
        if let fixtures {
            return APIClient(baseURL: baseURL, token: token, transport: FixtureTransport(directory: fixtures))
        }
        return APIClient(baseURL: baseURL, token: token)
    }

    /// Run an API call; a 401 signs the app out so the sidebar asks again.
    func call<T>(_ work: (APIClient) async throws -> T) async throws -> T {
        do {
            return try await work(client)
        } catch let error as APIError where error.isUnauthorized && token != nil {
            let name = activeName.map { " for \($0)" } ?? ""
            authProblem = "Living Brain refused the saved sign-in\(name). Sign in again."
            directory.removeActive()
            me = nil
            workspaceId = nil
            reloadDirectory()
            throw error
        }
    }

    /// The server URL changed: forget the in-memory token and load the one
    /// saved for the new server, if any.
    func serverChanged() {
        me = nil
        workspaceId = nil
        reloadDirectory()
    }

    // MARK: Workspaces

    /// Use another signed-in workspace.
    func switchTo(_ id: String) {
        guard id != activeId, directory.activate(id) else { return }
        me = nil
        workspaceId = nil
        authProblem = nil
        reloadDirectory()
    }

    /// Forget one workspace's sign-in on this Mac (the token stays valid on
    /// the server until revoked there).
    func remove(_ id: String) {
        let wasActive = id == activeId
        directory.remove(id)
        if wasActive {
            me = nil
            workspaceId = nil
        }
        reloadDirectory()
    }

    /// Keep a freshly issued token under the workspace it speaks for, and
    /// switch to it. When `/v1/me` cannot say which workspace that is, it is
    /// kept unlabelled and filed by the next `loadMe`.
    private func adopt(_ newToken: String) async throws {
        let probe = fixtures == nil ? APIClient(baseURL: baseURL, token: newToken) : client
        if let who = try? await probe.me() {
            try directory.save(token: newToken, for: SavedWorkspace(id: who.workspace.id, name: who.workspace.name))
            me = who
            workspaceId = who.workspace.id
        } else {
            try directory.saveUnlabelled(token: newToken)
            me = nil
            workspaceId = nil
        }
        authProblem = nil
        reloadDirectory()
    }

    // MARK: Sign in

    /// The device flow, in-app (the same endpoints and client id the CLI uses).
    func startSignIn() {
        signInTask?.cancel()
        authProblem = nil
        signInTask = Task {
            let anonymous = APIClient(baseURL: baseURL, token: nil)
            do {
                let authorization = try await anonymous.deviceAuthorize()
                signIn = .waiting(code: authorization.userCode, url: authorization.openURL)
                if let url = authorization.openURL { NSWorkspace.shared.open(url) }
                var interval = APIClient.pollInterval(authorization.interval)
                let deadline = Date().addingTimeInterval(TimeInterval(min(authorization.expiresIn ?? 600, 24 * 3600)))
                while Date() < deadline {
                    try await Task.sleep(nanoseconds: UInt64(interval) * 1_000_000_000)
                    switch try await anonymous.devicePoll(deviceCode: authorization.deviceCode) {
                    case .pending: continue
                    case .slowDown: interval += 5
                    case let .issued(newToken):
                        try await adopt(newToken)
                        signIn = .idle
                        return
                    }
                }
                signIn = .failed("The sign-in code expired. Start again.")
            } catch is CancellationError {
                signIn = .idle
            } catch {
                signIn = .failed(Scrub.text(error.localizedDescription))
            }
        }
    }

    func cancelSignIn() {
        signInTask?.cancel()
        signInTask = nil
        signIn = .idle
    }

    /// Reuse the CLI's sign-in: copy its keychain token (macOS asks the
    /// user to allow this app to read the CLI's item).
    func importCLISignIn() {
        let cli = KeychainTokenStore(service: KeychainTokenStore.cliService, account: KeychainTokenStore.account(for: baseURL))
        Task {
            do {
                guard let cliToken = try cli.read() else {
                    signIn = .failed("The CLI has no sign-in saved for \(KeychainTokenStore.account(for: baseURL)). Run `livingbrain login`, or sign in here.")
                    return
                }
                try await adopt(cliToken)
                signIn = .idle
            } catch {
                signIn = .failed(Scrub.text(error.localizedDescription))
            }
        }
    }

    /// Sign out of the workspace in use; another signed-in one takes over.
    func signOut() {
        directory.removeActive()
        workspaceId = nil
        me = nil
        reloadDirectory()
    }

    /// Reads who the token speaks for. A failure leaves the last answer (or
    /// none): it is a label, never a decision.
    func loadMe() async {
        guard signedIn else {
            me = nil
            return
        }
        let asked = activeId
        if let loaded = try? await call({ try await $0.me() }), asked == activeId {
            me = loaded
            workspaceId = loaded.workspace.id
            guard fixtures == nil else { return }
            let saved = SavedWorkspace(id: loaded.workspace.id, name: loaded.workspace.name)
            if activeId == nil {
                try? directory.label(saved)
                reloadDirectory()
            } else {
                directory.rename(saved)
                workspaces = directory.workspaces
            }
        }
    }
}
