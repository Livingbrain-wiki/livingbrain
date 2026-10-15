import AppKit
import Foundation
import LivingBrainKit

/// The signed-in side of the app: the API token and a client built on it.
/// The token lives in the keychain and in this object's memory, and only
/// ever travels in the `Authorization` header.
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
        } else {
            token = try? store.read()
        }
        signedIn = token != nil
    }

    var baseURL: URL { bar.serverURL }

    private var store: KeychainTokenStore {
        KeychainTokenStore(service: KeychainTokenStore.appService, account: KeychainTokenStore.account(for: baseURL))
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
            authProblem = "Living Brain refused the saved sign-in. Sign in again."
            token = nil
            signedIn = false
            try? store.delete()
            throw error
        }
    }

    /// The server URL changed: forget the in-memory token and load the one
    /// saved for the new server, if any.
    func serverChanged() {
        token = try? store.read()
        signedIn = token != nil
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
                        try store.write(newToken)
                        token = newToken
                        signedIn = true
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
        do {
            guard let cliToken = try cli.read() else {
                signIn = .failed("The CLI has no sign-in saved for \(KeychainTokenStore.account(for: baseURL)). Run `livingbrain login`, or sign in here.")
                return
            }
            try store.write(cliToken)
            token = cliToken
            signedIn = true
            signIn = .idle
            authProblem = nil
        } catch {
            signIn = .failed(Scrub.text(error.localizedDescription))
        }
    }

    func signOut() {
        try? store.delete()
        token = nil
        signedIn = false
        workspaceId = nil
    }
}
