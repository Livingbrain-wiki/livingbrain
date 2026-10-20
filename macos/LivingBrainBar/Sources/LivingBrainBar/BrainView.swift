import AppKit
import LivingBrainKit
import SwiftUI
import WebKit

/// The 3D brain: the web app's `/brain` page, shown in a web view. The page
/// and its scripts come from the server; the app's bearer token is attached
/// by `BrainSchemeHandler`, never by the page, so it is not in the URL, the
/// page's JavaScript, or any storage (`BrainWeb` has the rules).
struct BrainView: View {
    @ObservedObject var workspace: WorkspaceModel

    /// Where the view is: checking the token, showing the page, or saying why not.
    private enum Phase: Equatable {
        case checking
        case ready
        case signedOut(String)
        case failed(String)
    }

    @State private var phase: Phase = .checking
    @State private var attempt = 0

    var body: some View {
        Group {
            switch phase {
            case .checking:
                StatePlaceholder(kind: .loading, title: "Loading the brain…")
            case .ready:
                BrainWebView(workspace: workspace) { message in
                    phase = .failed(message)
                }
            case let .signedOut(message):
                VStack(spacing: 14) {
                    Image(systemName: "person.crop.circle.badge.questionmark")
                        .font(.system(size: 46)).foregroundStyle(.secondary)
                    Text("Sign in to see your brain").font(.title3.weight(.semibold))
                    Text(message).foregroundStyle(.secondary).multilineTextAlignment(.center).frame(maxWidth: 420)
                    Button("Sign In") { workspace.startSignIn() }
                        .buttonStyle(.borderedProminent)
                }
                .padding(40)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            case let .failed(message):
                StatePlaceholder(
                    kind: .error,
                    title: "The brain could not load",
                    message: message,
                    systemImage: "brain",
                    retry: { attempt += 1 }
                )
            }
        }
        .navigationTitle(Destination.brain.title)
        .task(id: "\(attempt)|\(workspace.activeId ?? "")|\(workspace.signedIn)") { await check() }
    }

    /// Reads the map once with the saved token. A refused token signs that
    /// workspace out (`WorkspaceModel.call`); a server that cannot be reached
    /// is an error with Try Again, not a blank page.
    private func check() async {
        guard workspace.signedIn else {
            phase = .signedOut(workspace.authProblem ?? "Sign in to Living Brain to open the 3D brain.")
            return
        }
        phase = .checking
        do {
            try await workspace.call { try await $0.graphProbe() }
            phase = .ready
        } catch let error as APIError where error.isUnauthorized {
            phase = .signedOut(workspace.authProblem ?? "Living Brain refused the saved sign-in. Sign in again.")
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }
}

/// Serves `lbbrain://` requests by fetching the same path from the server.
/// Only `GET`/`HEAD` under `/v1/` carry the token, and only to the server the
/// app is signed in to. Nothing is stored: no cookies, no cache.
@MainActor
final class BrainSchemeHandler: NSObject, WKURLSchemeHandler {
    struct Target {
        let server: URL
        let token: String?
    }

    private let target: () -> Target?
    private let session: URLSession
    private var inFlight: [ObjectIdentifier: (task: WKURLSchemeTask, work: Task<Void, Never>)] = [:]

    init(target: @escaping () -> Target?) {
        self.target = target
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.httpCookieAcceptPolicy = .never
        config.urlCache = nil
        config.timeoutIntervalForRequest = 30
        session = URLSession(configuration: config, delegate: BrainRedirectGuard(), delegateQueue: nil)
        super.init()
    }

    func webView(_ webView: WKWebView, start urlSchemeTask: WKURLSchemeTask) {
        let key = ObjectIdentifier(urlSchemeTask)
        let request = urlSchemeTask.request
        guard let url = request.url, let current = target() else {
            urlSchemeTask.didFailWithError(URLError(.userAuthenticationRequired))
            return
        }
        let method = request.httpMethod ?? "GET"
        let mode = BrainWeb.upstream(method: method, path: url.path)
        // The session check is answered from `/v1/me` and reshaped (see `BrainWeb.sessionShim`).
        let upstreamPath = BrainWeb.upstreamPath(method: method, path: url.path)
        let shim = upstreamPath != url.path
        var rewritten = URLComponents(url: url, resolvingAgainstBaseURL: false)
        rewritten?.path = upstreamPath
        guard mode != .refused, let rewrittenURL = rewritten?.url,
              let upstream = BrainWeb.serverURL(for: rewrittenURL, server: current.server) else {
            respondEmpty(urlSchemeTask, url: url, status: 405)
            return
        }
        var outgoing = URLRequest(url: upstream)
        outgoing.httpMethod = method
        outgoing.setValue(request.value(forHTTPHeaderField: "Accept") ?? "*/*", forHTTPHeaderField: "Accept")
        if mode == .bearer, let token = current.token, !token.isEmpty {
            outgoing.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        }
        let work = Task { @MainActor [weak self] in
            do {
                let (fetched, response) = try await self?.session.data(for: outgoing) ?? (Data(), URLResponse())
                guard let self, self.inFlight[key] != nil, let http = response as? HTTPURLResponse else { return }
                self.inFlight[key] = nil
                var data = fetched
                if shim, http.statusCode == 200 {
                    guard let reshaped = BrainWeb.sessionShim(fromMe: fetched) else {
                        respondEmpty(urlSchemeTask, url: url, status: 401)
                        return
                    }
                    data = reshaped
                }
                var headers: [String: String] = [:]
                for (name, value) in http.allHeaderFields {
                    if let name = name as? String, let value = value as? String, BrainWeb.passesHeader(name) {
                        headers[name] = value
                    }
                }
                guard let reply = HTTPURLResponse(url: url, statusCode: http.statusCode, httpVersion: "HTTP/1.1", headerFields: headers) else {
                    urlSchemeTask.didFailWithError(URLError(.cannotParseResponse))
                    return
                }
                urlSchemeTask.didReceive(reply)
                if !data.isEmpty { urlSchemeTask.didReceive(data) }
                urlSchemeTask.didFinish()
            } catch {
                guard let self, self.inFlight.removeValue(forKey: key) != nil else { return }
                urlSchemeTask.didFailWithError(error)
            }
        }
        inFlight[key] = (urlSchemeTask, work)
    }

    func webView(_ webView: WKWebView, stop urlSchemeTask: WKURLSchemeTask) {
        // WebKit stopped the load: cancel the fetch and never call the task again.
        guard let entry = inFlight.removeValue(forKey: ObjectIdentifier(urlSchemeTask)) else { return }
        entry.work.cancel()
    }

    private func respondEmpty(_ task: WKURLSchemeTask, url: URL, status: Int) {
        guard let reply = HTTPURLResponse(url: url, statusCode: status, httpVersion: "HTTP/1.1", headerFields: nil) else {
            task.didFailWithError(URLError(.cannotParseResponse))
            return
        }
        task.didReceive(reply)
        task.didFinish()
    }
}

/// Follows redirects only to the same origin the request was made to.
private final class BrainRedirectGuard: NSObject, URLSessionTaskDelegate {
    func urlSession(
        _ session: URLSession,
        task: URLSessionTask,
        willPerformHTTPRedirection response: HTTPURLResponse,
        newRequest request: URLRequest,
        completionHandler: @escaping (URLRequest?) -> Void
    ) {
        guard let original = task.originalRequest?.url, let next = request.url, BrainWeb.sameOrigin(original, next) else {
            completionHandler(nil)
            return
        }
        completionHandler(request)
    }
}

/// The web view. Non-persistent storage, navigation held to the brain page,
/// everything else opened in the default browser.
struct BrainWebView: NSViewRepresentable {
    let workspace: WorkspaceModel
    let onFailure: (String) -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(workspace: workspace, onFailure: onFailure)
    }

    func makeNSView(context: Context) -> WKWebView {
        let config = WKWebViewConfiguration()
        config.websiteDataStore = .nonPersistent()
        let handler = BrainSchemeHandler { [weak workspace] in
            guard let workspace, let token = workspace.accessToken else { return nil }
            return BrainSchemeHandler.Target(server: workspace.baseURL, token: token)
        }
        context.coordinator.handler = handler
        config.setURLSchemeHandler(handler, forURLScheme: BrainWeb.scheme)
        let view = WKWebView(frame: .zero, configuration: config)
        view.navigationDelegate = context.coordinator
        view.uiDelegate = context.coordinator
        view.load(URLRequest(url: BrainWeb.entryURL))
        return view
    }

    func updateNSView(_ view: WKWebView, context: Context) {}

    @MainActor
    final class Coordinator: NSObject, WKNavigationDelegate, WKUIDelegate {
        let workspace: WorkspaceModel
        let onFailure: (String) -> Void
        var handler: BrainSchemeHandler?

        init(workspace: WorkspaceModel, onFailure: @escaping (String) -> Void) {
            self.workspace = workspace
            self.onFailure = onFailure
        }

        func webView(
            _ webView: WKWebView,
            decidePolicyFor action: WKNavigationAction,
            decisionHandler: @escaping (WKNavigationActionPolicy) -> Void
        ) {
            guard let url = action.request.url else {
                decisionHandler(.cancel)
                return
            }
            switch BrainWeb.navigation(to: url, server: workspace.baseURL) {
            case .allow:
                decisionHandler(.allow)
            case let .browser(external):
                NSWorkspace.shared.open(external)
                decisionHandler(.cancel)
            case .cancel:
                decisionHandler(.cancel)
            }
        }

        func webView(_ webView: WKWebView, createWebViewWith configuration: WKWebViewConfiguration,
                     for navigationAction: WKNavigationAction, windowFeatures: WKWindowFeatures) -> WKWebView? {
            // target="_blank" and window.open: the default browser, never a second view.
            if let url = navigationAction.request.url, case let .browser(external) = BrainWeb.navigation(to: url, server: workspace.baseURL) {
                NSWorkspace.shared.open(external)
            }
            return nil
        }

        func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
            fail(error)
        }

        func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
            fail(error)
        }

        private func fail(_ error: Error) {
            if (error as NSError).code == NSURLErrorCancelled { return }
            onFailure("Could not load the brain page (\(error.localizedDescription)).")
        }
    }
}

