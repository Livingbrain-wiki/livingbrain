//! Part C end to end, against a fake GitHub: the manifest flow, the
//! install callback, the webhook, the settings routes and the cron drain.
//! No network: every call goes to [`FakeGithub`], which also logs every
//! call so a test can prove what was (and was not) asked of GitHub.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use cratefield_core::axum::body::{Body, Bytes};
use cratefield_core::axum::http::{self, Method, Request, StatusCode, header};
use cratefield_core::{
    Database, HttpClient, HttpError, Kid, MapConfig, Module, Payload, Ports, ScheduledBudget,
    Signer, Statement, UlidIdGen,
};
use cratefield_kms::Kms;
use cratefield_testing::{MemoryBlob, TestHarness};
use hmac::{Hmac, Mac};
use livingbrain_pages::Pages;
use livingbrain_workspaces::Workspaces;
use serde_json::{Value, json};
use sha2::Sha256;
use tower::ServiceExt;

use crate::common::{get_int, get_text, int, text};
use crate::sync::DrainCursor;
use crate::{BackfillPhase, GithubApp, InstallationStatus, StatePurpose};

const W1: &str = "W1SPACE";
const W2: &str = "W2SPACE";
const OWNER: &str = "U1OWNER";
const PLAIN: &str = "U1PLAIN";
const OWNER2: &str = "U2OWNER";
const APP1: i64 = 1001;
const APP2: i64 = 2002;
const INST1: i64 = 5001;
const INST2: i64 = 6002;
const REPO1: i64 = 77_001;
const REPO2: i64 = 88_002;
const NOW: i64 = 1_800_000_000;
const BASE: &str = "https://app.livingbrain.wiki";

const KEY_FILE: &str = include_str!("../testdata/test-app-key.pem");

fn pem() -> String {
    KEY_FILE[KEY_FILE.find("-----BEGIN").expect("a PEM")..].to_owned()
}

fn webhook_secret(app_id: i64) -> String {
    format!("WEBHOOK-SECRET-{app_id}-0123456789")
}

fn client_secret(app_id: i64) -> String {
    format!("CLIENT-SECRET-{app_id}-9876543210")
}

fn token_for(installation_id: i64) -> String {
    format!("ghs_TOKEN_{installation_id}_abcdef")
}

fn kms() -> Arc<dyn Kms> {
    let key = STANDARD.encode([9_u8; 32]);
    Arc::new(
        cratefield_kms::WorkerSecretKms::from_lookup(|name| match name {
            "HARNESS_KEK_CURRENT" => Some("1".to_owned()),
            "HARNESS_KEK_V1" => Some(key.clone()),
            _ => None,
        })
        .expect("the key ring is well formed"),
    )
}

// ---------------------------------------------------------------------------
// The fake GitHub

#[derive(Clone)]
struct FakeItem {
    number: u64,
    pr: bool,
    comments: usize,
    reviews: usize,
}

#[derive(Clone)]
struct FakeRepo {
    id: i64,
    full_name: String,
    installation: i64,
    commits: usize,
    items: Vec<FakeItem>,
}

/// A scripted answer: the path needle, the status, the headers.
type Scripted = (String, u16, Vec<(String, String)>);

#[derive(Default)]
struct GhState {
    /// manifest code → app id.
    codes: HashMap<String, i64>,
    /// installation → (app id, account login).
    installations: HashMap<i64, (i64, String)>,
    repos: Vec<FakeRepo>,
    /// Scripted answers: the next call whose path contains the needle gets
    /// this status and these headers instead.
    overrides: VecDeque<Scripted>,
    /// `METHOD path` of every call, in order.
    log: Vec<String>,
}

#[derive(Clone, Default)]
struct FakeGithub(Arc<Mutex<GhState>>);

impl FakeGithub {
    fn add_code(&self, code: &str, app_id: i64) {
        self.0.lock().unwrap().codes.insert(code.to_owned(), app_id);
    }

    fn add_installation(&self, installation_id: i64, app_id: i64, login: &str) {
        self.0
            .lock()
            .unwrap()
            .installations
            .insert(installation_id, (app_id, login.to_owned()));
    }

    fn add_repo(&self, repo: FakeRepo) {
        self.0.lock().unwrap().repos.push(repo);
    }

    fn set_commits(&self, repo_id: i64, commits: usize) {
        let mut state = self.0.lock().unwrap();
        if let Some(repo) = state.repos.iter_mut().find(|r| r.id == repo_id) {
            repo.commits = commits;
        }
    }

    fn script(&self, needle: &str, status: u16, headers: &[(&str, &str)]) {
        self.0.lock().unwrap().overrides.push_back((
            needle.to_owned(),
            status,
            headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        ));
    }

    fn log(&self) -> Vec<String> {
        self.0.lock().unwrap().log.clone()
    }

    fn clear_log(&self) {
        self.0.lock().unwrap().log.clear();
    }
}

fn reply(status: u16, body: &Value) -> http::Response<Bytes> {
    http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .header("x-ratelimit-remaining", "4000")
        .header("x-ratelimit-reset", (NOW + 3600).to_string())
        .body(Bytes::from(body.to_string()))
        .unwrap()
}

fn paged(status: u16, body: &Value, path: &str, page: usize, more: bool) -> http::Response<Bytes> {
    let mut response = reply(status, body);
    if more {
        let base = path.split('?').next().unwrap_or(path);
        let link = format!(
            "<https://api.github.com{base}?per_page=100&page={}>; rel=\"next\"",
            page + 1
        );
        response
            .headers_mut()
            .insert("link", http::HeaderValue::from_str(&link).unwrap());
    }
    response
}

/// The `iss` of a JWT, read without verifying (the fake trusts the shape;
/// the signer's own tests cover the signature).
fn jwt_issuer(authorization: &str) -> Option<i64> {
    let jwt = authorization.strip_prefix("Bearer ")?;
    let mut parts = jwt.split('.');
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts.next()?).ok()?).ok()?;
    if header["alg"] != "RS256" {
        return None;
    }
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts.next()?).ok()?).ok()?;
    let signature = parts.next()?;
    if signature.is_empty() || claims["exp"].as_i64()? <= claims["iat"].as_i64()? {
        return None;
    }
    claims["iss"].as_i64()
}

fn query_page(query: &str) -> usize {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("page="))
        .and_then(|page| page.parse().ok())
        .unwrap_or(1)
}

fn sha(n: usize) -> String {
    format!("{n:040x}")
}

fn item_json(repo: &FakeRepo, item: &FakeItem) -> Value {
    json!({
        "number": item.number,
        "title": format!("Item {}", item.number),
        "body": "the body",
        "state": "open",
        "html_url": format!("https://github.com/{}/issues/{}", repo.full_name, item.number),
        "user": {"login": "octo"},
        "labels": [],
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": format!("2026-02-{:02}T00:00:00Z", (item.number % 27) + 1),
        "closed_at": null,
        "comments": item.comments,
        "pull_request": if item.pr { json!({"merged_at": null}) } else { Value::Null },
    })
}

impl GhState {
    fn route(&mut self, method: &Method, path: &str, auth: &str) -> http::Response<Bytes> {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        let segments: Vec<&str> = route.trim_start_matches('/').split('/').collect();
        match (method.as_str(), segments.as_slice()) {
            ("POST", ["app-manifests", code, "conversions"]) => match self.codes.remove(*code) {
                Some(app_id) => reply(
                    201,
                    &json!({
                        "id": app_id,
                        "slug": format!("living-brain-{app_id}"),
                        "client_id": format!("Iv1.{app_id}"),
                        "client_secret": client_secret(app_id),
                        "webhook_secret": webhook_secret(app_id),
                        "pem": pem(),
                        "owner": {"login": "World-360", "type": "Organization"},
                    }),
                ),
                None => reply(404, &json!({"message": "Not Found"})),
            },
            ("GET", ["app", "installations", id]) => {
                let id: i64 = id.parse().unwrap_or_default();
                match (self.installations.get(&id), jwt_issuer(auth)) {
                    (Some((app_id, login)), Some(iss)) if *app_id == iss => reply(
                        200,
                        &json!({
                            "id": id, "app_id": app_id,
                            "account": {"login": login, "type": "Organization"},
                            "suspended_at": null,
                        }),
                    ),
                    _ => reply(404, &json!({"message": "Not Found"})),
                }
            }
            ("POST", ["app", "installations", id, "access_tokens"]) => {
                let id: i64 = id.parse().unwrap_or_default();
                match (self.installations.get(&id), jwt_issuer(auth)) {
                    (Some((app_id, _)), Some(iss)) if *app_id == iss => {
                        reply(201, &json!({"token": token_for(id), "expires_at": "x"}))
                    }
                    _ => reply(404, &json!({"message": "Not Found"})),
                }
            }
            ("GET", ["installation", "repositories"]) => {
                let Some(inst) = self.installation_of(auth) else {
                    return reply(401, &json!({"message": "Bad credentials"}));
                };
                let repos: Vec<Value> = self
                    .repos
                    .iter()
                    .filter(|r| r.installation == inst)
                    .map(|r| json!({"id": r.id, "full_name": r.full_name}))
                    .collect();
                reply(
                    200,
                    &json!({"total_count": repos.len(), "repositories": repos}),
                )
            }
            ("GET", ["repos", owner, name, rest @ ..]) => {
                let full = format!("{owner}/{name}");
                let inst = self.installation_of(auth);
                let Some(repo) = self
                    .repos
                    .iter()
                    .find(|r| r.full_name == full && Some(r.installation) == inst)
                    .cloned()
                else {
                    return reply(404, &json!({"message": "Not Found"}));
                };
                let page = query_page(query);
                match rest {
                    ["commits"] => {
                        let start = (page - 1) * 100;
                        let end = (start + 100).min(repo.commits);
                        let list: Vec<Value> = (start..end)
                            .map(|i| {
                                json!({
                                    "sha": sha(repo.commits - i),
                                    "html_url": format!("https://github.com/{full}/commit/{}", sha(repo.commits - i)),
                                    "commit": {"message": "m", "author": {"name": "N", "email": "n@example.com", "date": "d"}},
                                    "author": {"login": "octo"},
                                })
                            })
                            .collect();
                        paged(200, &json!(list), path, page, end < repo.commits)
                    }
                    ["commits", sha] => reply(
                        200,
                        &json!({
                            "sha": sha,
                            "html_url": format!("https://github.com/{full}/commit/{sha}"),
                            "commit": {"message": format!("commit {sha}"), "author": {"name": "N", "email": "n@example.com", "date": "d"}},
                            "author": {"login": "octo"},
                            "files": [{"filename": "a.rs", "status": "modified", "additions": 1, "deletions": 0, "patch": "@@"}],
                            "stats": {"additions": 1, "deletions": 0, "total": 1},
                        }),
                    ),
                    ["issues"] => {
                        let start = (page - 1) * 100;
                        let end = (start + 100).min(repo.items.len());
                        let list: Vec<Value> = repo.items[start..end]
                            .iter()
                            .map(|item| item_json(&repo, item))
                            .collect();
                        paged(200, &json!(list), path, page, end < repo.items.len())
                    }
                    ["issues", n] => match repo.items.iter().find(|i| i.number.to_string() == *n) {
                        Some(item) => reply(200, &item_json(&repo, item)),
                        None => reply(404, &json!({"message": "Not Found"})),
                    },
                    ["issues", n, "comments"] => {
                        let count = repo
                            .items
                            .iter()
                            .find(|i| i.number.to_string() == *n)
                            .map_or(0, |i| i.comments);
                        let list: Vec<Value> = (0..count)
                            .map(|c| {
                                json!({"id": c, "user": {"login": "octo"}, "body": format!("comment {c}"),
                                       "created_at": format!("2026-03-01T00:00:{:02}Z", c % 60), "html_url": null})
                            })
                            .collect();
                        reply(200, &json!(list))
                    }
                    ["pulls", n, "reviews"] => {
                        let count = repo
                            .items
                            .iter()
                            .find(|i| i.number.to_string() == *n)
                            .map_or(0, |i| i.reviews);
                        let list: Vec<Value> = (0..count)
                            .map(|c| {
                                json!({"id": c, "user": {"login": "rev"}, "body": "lgtm", "state": "APPROVED",
                                       "submitted_at": "2026-03-02T00:00:00Z", "html_url": null})
                            })
                            .collect();
                        reply(200, &json!(list))
                    }
                    _ => reply(404, &json!({"message": "Not Found"})),
                }
            }
            _ => reply(404, &json!({"message": "Not Found"})),
        }
    }

    fn installation_of(&self, auth: &str) -> Option<i64> {
        let token = auth.strip_prefix("Bearer ")?;
        self.installations
            .keys()
            .copied()
            .find(|inst| token == token_for(*inst))
    }
}

#[async_trait::async_trait]
impl HttpClient for FakeGithub {
    async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let uri = request.uri().to_string();
        let Some(path) = uri.strip_prefix("https://api.github.com") else {
            return Err(HttpError::Transport("not GitHub".to_owned()));
        };
        let auth = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert_eq!(
            request.headers().get("x-github-api-version").unwrap(),
            "2022-11-28"
        );
        assert!(request.headers().get("user-agent").is_some());
        let mut state = self.0.lock().unwrap();
        state.log.push(format!("{} {path}", request.method()));
        if let Some(index) = state
            .overrides
            .iter()
            .position(|(needle, _, _)| path.contains(needle.as_str()))
        {
            let (_, status, headers) = state.overrides.remove(index).unwrap();
            let mut response = http::Response::builder().status(status);
            for (k, v) in headers {
                response = response.header(k, v);
            }
            return Ok(response
                .body(Bytes::from_static(b"{\"message\":\"scripted\"}"))
                .unwrap());
        }
        Ok(state.route(request.method(), path, &auth))
    }
}

// ---------------------------------------------------------------------------
// The kit

struct Kit {
    kit: TestHarness,
    gh: FakeGithub,
    blob: Arc<MemoryBlob>,
    /// Every response body and `Location` this test saw, for the
    /// no-secrets sweep.
    seen: Mutex<Vec<String>>,
}

struct Res {
    status: StatusCode,
    location: Option<String>,
    body: Vec<u8>,
}

impl Res {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).expect("JSON body")
    }
}

fn kit() -> Kit {
    let gh = FakeGithub::default();
    let blob = Arc::new(MemoryBlob::new());
    let (http, store) = (gh.clone(), blob.clone());
    let kit = TestHarness::with_ports(
        vec![
            Box::new(Workspaces::new()),
            Box::new(Pages::new()),
            Box::new(GithubApp::new().with_kms(kms())),
        ],
        move |ports| {
            ports.http = Some(Arc::new(http));
            ports.blob = Some(store);
            ports.config = Arc::new(MapConfig::from_pairs([("WORKSPACES_REDIRECT_BASE", BASE)]));
        },
    );
    Kit {
        kit,
        gh,
        blob,
        seen: Mutex::new(Vec::new()),
    }
}

impl Kit {
    fn db(&self) -> &dyn Database {
        &*self.kit.db
    }

    fn cookie(&self, workspace: &str, user: &str) -> String {
        self.kit.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({"team_id": workspace, "user_id": user, "exp": NOW + 3600}).to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        cookie: Option<&str>,
        headers: &[(&str, String)],
        body: Vec<u8>,
    ) -> Res {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(cookie) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("{}={cookie}", livingbrain_workspaces::SESSION_COOKIE),
            );
        }
        for (name, value) in headers {
            builder = builder.header(*name, value.as_str());
        }
        let request = builder
            .header(header::CONTENT_LENGTH, body.len().to_string())
            .body(Body::from(body))
            .unwrap();
        let response = self.kit.router.clone().oneshot(request).await.unwrap();
        let (parts, body) = response.into_parts();
        let body = cratefield_core::axum::body::to_bytes(body, 4 * 1024 * 1024)
            .await
            .unwrap()
            .to_vec();
        let location = parts
            .headers
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let mut seen = self.seen.lock().unwrap();
        seen.push(String::from_utf8_lossy(&body).into_owned());
        if let Some(location) = &location {
            seen.push(location.clone());
        }
        Res {
            status: parts.status,
            location,
            body,
        }
    }

    async fn get(&self, path: &str, cookie: Option<&str>) -> Res {
        self.send(Method::GET, path, cookie, &[], Vec::new()).await
    }

    async fn write(&self, method: Method, path: &str, cookie: &str, body: Value) -> Res {
        self.send(
            method,
            path,
            Some(cookie),
            &[],
            body.to_string().into_bytes(),
        )
        .await
    }

    /// A delivery signed with `secret`.
    async fn deliver(
        &self,
        app_id: i64,
        event: &str,
        delivery: &str,
        body: &Value,
        secret: &str,
    ) -> Res {
        let raw = body.to_string().into_bytes();
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(&raw);
        let signature = format!(
            "sha256={}",
            crate::common::hex(&mac.finalize().into_bytes())
        );
        self.send(
            Method::POST,
            "/v1/github/webhook",
            None,
            &[
                ("x-github-event", event.to_owned()),
                ("x-github-delivery", delivery.to_owned()),
                ("x-hub-signature-256", signature),
                (
                    "x-github-hook-installation-target-type",
                    "integration".to_owned(),
                ),
                ("x-github-hook-installation-target-id", app_id.to_string()),
                ("content-type", "application/json".to_owned()),
            ],
            raw,
        )
        .await
    }

    /// Runs the module's cron entry point with a budget of `steps` calls.
    async fn tick(&self, steps: u32) {
        let module: Arc<dyn Module> = self
            .kit
            .modules
            .iter()
            .find(|m| m.name() == "github")
            .cloned()
            .unwrap();
        let mut ports = Ports::with_config(Arc::new(MapConfig::from_pairs([(
            "WORKSPACES_REDIRECT_BASE",
            BASE,
        )])));
        ports.db = Some(self.kit.db.clone());
        ports.clock = Some(Arc::new(self.kit.clock.clone()));
        ports.http = Some(Arc::new(self.gh.clone()));
        ports.blob = Some(self.blob.clone());
        ports.id_gen = Some(Arc::new(UlidIdGen));
        ports.defer = Some(Arc::new(self.kit.defer.clone()));
        ports.signer = Some(self.kit.signer.clone());
        let mut ctx = self.kit.harness.module_context(module.as_ref(), &ports);
        ctx.scheduled = Arc::new(ScheduledBudget::new(None, Some(steps)));
        module
            .scheduled(&ctx, "*/15 * * * *")
            .await
            .expect("the drain runs");
    }

    async fn seed(&self) {
        for (id, owner) in [(W1, OWNER), (W2, OWNER2)] {
            self.db()
                .execute(&Statement::with_values(
                    "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?)",
                    vec![
                        text(id),
                        text("World-360"),
                        text(owner),
                        text("2026-01-01T00:00:00Z"),
                    ],
                ))
                .await
                .unwrap();
        }
        for (ws, user, role) in [
            (W1, OWNER, "owner"),
            (W1, PLAIN, "member"),
            (W2, OWNER2, "owner"),
        ] {
            self.db()
                .execute(&Statement::with_values(
                    "INSERT INTO workspace_members (workspace_id, user_id, name, is_admin, updated_at, role) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                    vec![
                        text(ws),
                        text(user),
                        text("someone"),
                        int(i64::from(role != "member")),
                        text("2026-01-01T00:00:00Z"),
                        text(role),
                    ],
                ))
                .await
                .unwrap();
        }
    }

    /// The whole connect flow for one workspace, asserting each hop.
    async fn connect(
        &self,
        ws: &str,
        member: &str,
        app_id: i64,
        installation_id: i64,
        repo: FakeRepo,
    ) {
        let cookie = self.cookie(ws, member);
        let started = self
            .write(
                Method::POST,
                "/v1/github/app/manifest",
                &cookie,
                json!({"owner": "org", "org": "World-360"}),
            )
            .await;
        assert_eq!(
            started.status,
            StatusCode::OK,
            "{}",
            String::from_utf8_lossy(&started.body)
        );
        let started = started.json();
        let action = started["action"].as_str().unwrap();
        assert!(
            action
                .starts_with("https://github.com/organizations/World-360/settings/apps/new?state=")
        );
        assert_eq!(started["field"], "manifest");
        assert_eq!(
            started["manifest"]["hook_attributes"]["url"],
            format!("{BASE}/v1/github/webhook")
        );
        let state = action.split_once("state=").unwrap().1.to_owned();

        let code = format!("code-{app_id}");
        self.gh.add_code(&code, app_id);
        let back = self
            .get(
                &format!("/v1/github/app/callback?code={code}&state={state}"),
                Some(&cookie),
            )
            .await;
        assert_eq!(back.status, StatusCode::FOUND);
        let location = back.location.unwrap();
        let prefix =
            format!("https://github.com/apps/living-brain-{app_id}/installations/new?state=");
        assert!(location.starts_with(&prefix), "{location}");
        let install_state = location[prefix.len()..].to_owned();

        self.gh
            .add_installation(installation_id, app_id, "World-360");
        self.gh.add_repo(repo);
        let done = self
            .get(
                &format!("/v1/github/install/callback?installation_id={installation_id}&setup_action=install&state={install_state}"),
                Some(&cookie),
            )
            .await;
        assert_eq!(done.status, StatusCode::FOUND);
        assert_eq!(
            done.location.as_deref(),
            Some("/settings.html?github=installed#github")
        );
    }

    async fn connected(&self) {
        self.seed().await;
        self.connect(W1, OWNER, APP1, INST1, earthos()).await;
        self.connect(W2, OWNER2, APP2, INST2, other_repo()).await;
    }

    async fn sources(&self) -> Vec<(String, String, String, String)> {
        let rows = self
            .db()
            .query(&Statement::new(
                "SELECT kind, scope, rel_path, COALESCE(project, '') AS project FROM sources ORDER BY rel_path",
            ))
            .await
            .unwrap();
        rows.rows
            .iter()
            .map(|r| {
                (
                    get_text(r, "kind"),
                    get_text(r, "scope"),
                    get_text(r, "rel_path"),
                    get_text(r, "project"),
                )
            })
            .collect()
    }

    async fn queue_len(&self) -> i64 {
        let rows = self
            .db()
            .query(&Statement::new("SELECT COUNT(*) AS n FROM github_queue"))
            .await
            .unwrap();
        get_int(rows.first().unwrap(), "n")
    }

    fn assert_no_secrets(&self) {
        let key_body = pem().lines().nth(1).unwrap().to_owned();
        let seen = self.seen.lock().unwrap();
        for text in seen.iter() {
            for secret in [
                key_body.as_str(),
                &webhook_secret(APP1),
                &webhook_secret(APP2),
                &client_secret(APP1),
                &client_secret(APP2),
                &token_for(INST1),
                &token_for(INST2),
                "BEGIN RSA",
            ] {
                assert!(
                    !text.contains(secret),
                    "a response carried a secret: {text}"
                );
            }
        }
    }
}

fn earthos() -> FakeRepo {
    FakeRepo {
        id: REPO1,
        full_name: "EarthOS-world/EarthOS.world".to_owned(),
        installation: INST1,
        commits: 3,
        items: vec![
            FakeItem {
                number: 1,
                pr: false,
                comments: 0,
                reviews: 0,
            },
            FakeItem {
                number: 2,
                pr: false,
                comments: 2,
                reviews: 0,
            },
            FakeItem {
                number: 3,
                pr: true,
                comments: 1,
                reviews: 1,
            },
        ],
    }
}

fn other_repo() -> FakeRepo {
    FakeRepo {
        id: REPO2,
        full_name: "Other-org/secret-plans".to_owned(),
        installation: INST2,
        commits: 1,
        items: vec![],
    }
}

// ---------------------------------------------------------------------------
// The manifest and install flow

#[test]
fn the_manifest_flow_creates_seals_installs_and_lists_repositories() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        let cookie = k.cookie(W1, OWNER);
        let summary = k.get("/v1/github", Some(&cookie)).await;
        assert_eq!(summary.status, StatusCode::OK);
        let summary = summary.json();
        assert_eq!(summary["app"]["app_id"], APP1);
        assert_eq!(summary["app"]["slug"], format!("living-brain-{APP1}"));
        assert_eq!(summary["installations"][0]["installation_id"], INST1);
        assert_eq!(summary["installations"][0]["status"], "active");
        assert_eq!(summary["installations"][0]["bound_by"], OWNER);
        let repos = summary["repos"].as_array().unwrap();
        assert_eq!(
            repos.len(),
            1,
            "only this workspace's repository: {repos:?}"
        );
        assert_eq!(repos[0]["full_name"], "EarthOS-world/EarthOS.world");
        assert_eq!(repos[0]["project"], "EarthOS.world");
        assert_eq!(repos[0]["scope"], "shared");
        assert_eq!(repos[0]["backfill"]["state"], "pending");
        // The credentials are sealed: the plaintext is nowhere in the row.
        let rows = k
            .db()
            .query(&Statement::new("SELECT * FROM github_app"))
            .await
            .unwrap();
        let dump = format!("{:?}", rows.rows);
        assert!(!dump.contains(&webhook_secret(APP1)));
        assert!(!dump.contains("BEGIN RSA"));
        // The flow made the conversion, the confirmation, the token and the
        // repository list — and read no history.
        assert!(
            k.gh.log().iter().all(|call| !call.contains("/repos/")),
            "{:?}",
            k.gh.log()
        );
        k.assert_no_secrets();
    });
}

#[test]
fn a_plain_member_cannot_start_and_a_second_app_is_refused() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        let plain = k
            .write(
                Method::POST,
                "/v1/github/app/manifest",
                &k.cookie(W1, PLAIN),
                json!({}),
            )
            .await;
        assert_eq!(plain.status, StatusCode::FORBIDDEN);
        let again = k
            .write(
                Method::POST,
                "/v1/github/app/manifest",
                &k.cookie(W1, OWNER),
                json!({}),
            )
            .await;
        assert_eq!(again.status, StatusCode::CONFLICT);
        // A cookie write from another site is refused before anything else.
        let cross = k
            .send(
                Method::POST,
                "/v1/github/app/manifest",
                Some(&k.cookie(W1, OWNER)),
                &[("sec-fetch-site", "cross-site".to_owned())],
                b"{}".to_vec(),
            )
            .await;
        assert_eq!(cross.status, StatusCode::FORBIDDEN);
        let anonymous = k.get("/v1/github", None).await;
        assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
    });
}

#[test]
fn a_bad_expired_reused_or_foreign_state_is_refused() {
    pollster::block_on(async {
        let k = kit();
        k.seed().await;
        let cookie = k.cookie(W1, OWNER);
        k.gh.add_code("code-1", APP1);

        // Unknown state.
        let bad = k
            .get(
                "/v1/github/app/callback?code=code-1&state=nope",
                Some(&cookie),
            )
            .await;
        assert_eq!(
            bad.location.as_deref(),
            Some("/settings.html?github=error&reason=state#github")
        );

        // Expired: minted long before the callback.
        let now = time::OffsetDateTime::from_unix_timestamp(NOW).unwrap();
        let old = crate::create_state(
            k.db(),
            now - time::Duration::hours(2),
            StatePurpose::Manifest,
            W1,
            OWNER,
            60,
        )
        .await
        .unwrap();
        let expired = k
            .get(
                &format!("/v1/github/app/callback?code=code-1&state={old}"),
                Some(&cookie),
            )
            .await;
        assert_eq!(
            expired.location.as_deref(),
            Some("/settings.html?github=error&reason=state#github")
        );

        // Another member's state (W2's owner presents W1's token).
        let started = k
            .write(Method::POST, "/v1/github/app/manifest", &cookie, json!({}))
            .await
            .json();
        let state = started["action"]
            .as_str()
            .unwrap()
            .split_once("state=")
            .unwrap()
            .1
            .to_owned();
        let foreign = k
            .get(
                &format!("/v1/github/app/callback?code=code-1&state={state}"),
                Some(&k.cookie(W2, OWNER2)),
            )
            .await;
        assert_eq!(
            foreign.location.as_deref(),
            Some("/settings.html?github=error&reason=state#github")
        );
        // ... and it was spent by the misuse: the rightful owner cannot reuse it.
        let reused = k
            .get(
                &format!("/v1/github/app/callback?code=code-1&state={state}"),
                Some(&cookie),
            )
            .await;
        assert_eq!(
            reused.location.as_deref(),
            Some("/settings.html?github=error&reason=state#github")
        );

        // Signed out.
        let signed_out = k
            .get(
                &format!("/v1/github/app/callback?code=code-1&state={state}"),
                None,
            )
            .await;
        assert_eq!(
            signed_out.location.as_deref(),
            Some("/settings.html?github=error&reason=signed-out#github")
        );
        // A code that is not a plain token never reaches a URL.
        let odd = k
            .get("/v1/github/app/callback?code=..%2Fx&state=s", Some(&cookie))
            .await;
        assert_eq!(
            odd.location.as_deref(),
            Some("/settings.html?github=error&reason=bad-request#github")
        );

        // No app was saved, and no conversion was ever attempted.
        assert!(crate::app_summary(k.db(), W1).await.unwrap().is_none());
        assert!(
            k.gh.log().iter().all(|c| !c.contains("app-manifests")),
            "{:?}",
            k.gh.log()
        );
    });
}

#[test]
fn an_installation_is_bound_only_after_github_confirms_it() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        let cookie = k.cookie(W1, OWNER);
        let install_state = || async {
            let started = k
                .write(Method::POST, "/v1/github/app/install", &cookie, json!({}))
                .await
                .json();
            started["url"]
                .as_str()
                .unwrap()
                .split_once("state=")
                .unwrap()
                .1
                .to_owned()
        };
        // An id GitHub does not know.
        let state = install_state().await;
        let forged = k
            .get(&format!("/v1/github/install/callback?installation_id=999999&setup_action=install&state={state}"), Some(&cookie))
            .await;
        assert_eq!(
            forged.location.as_deref(),
            Some("/settings.html?github=error&reason=not-confirmed#github")
        );
        // W2's installation: GitHub refuses W1's app JWT for it, so W1 can
        // neither take it nor squat on its id.
        let state = install_state().await;
        let stolen = k
            .get(&format!("/v1/github/install/callback?installation_id={INST2}&setup_action=install&state={state}"), Some(&cookie))
            .await;
        assert_eq!(
            stolen.location.as_deref(),
            Some("/settings.html?github=error&reason=not-confirmed#github")
        );
        let rows = k
            .db()
            .query(&Statement::with_values(
                "SELECT workspace_id, app_id FROM github_installations WHERE installation_id IN (?, ?)",
                vec![int(999_999), int(INST2)],
            ))
            .await
            .unwrap();
        assert_eq!(rows.rows.len(), 1, "only W2's own row exists");
        assert_eq!(get_text(&rows.rows[0], "workspace_id"), W2);
        assert_eq!(get_int(&rows.rows[0], "app_id"), APP2);
        // An approval request carries no installation yet.
        let requested = k
            .get(
                "/v1/github/install/callback?setup_action=request",
                Some(&cookie),
            )
            .await;
        assert_eq!(
            requested.location.as_deref(),
            Some("/settings.html?github=requested#github")
        );
    });
}

// ---------------------------------------------------------------------------
// The webhook

fn issue_event(installation: i64, repo_id: i64, full_name: &str, number: u64) -> Value {
    json!({
        "action": "edited",
        "issue": {"number": number},
        "repository": {"id": repo_id, "full_name": full_name},
        "installation": {"id": installation},
    })
}

#[test]
fn a_webhook_verifies_before_it_is_read_and_a_replay_is_a_no_op() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        let body = issue_event(INST1, REPO1, "EarthOS-world/EarthOS.world", 2);
        let secret = webhook_secret(APP1);

        let ok = k.deliver(APP1, "issues", "aaaa-0001", &body, &secret).await;
        assert_eq!(ok.status, StatusCode::OK);
        assert_eq!(ok.json()["handled"], "queued");
        assert_eq!(k.queue_len().await, 1);

        let replay = k.deliver(APP1, "issues", "aaaa-0001", &body, &secret).await;
        assert_eq!(replay.json()["handled"], "replay");
        assert_eq!(k.queue_len().await, 1);

        let forged = k
            .deliver(APP1, "issues", "aaaa-0002", &body, "not-the-secret")
            .await;
        assert_eq!(forged.status, StatusCode::UNAUTHORIZED);
        // W2's secret does not speak for W1's app.
        let crossed = k
            .deliver(APP1, "issues", "aaaa-0003", &body, &webhook_secret(APP2))
            .await;
        assert_eq!(crossed.status, StatusCode::UNAUTHORIZED);
        let unknown = k.deliver(4242, "issues", "aaaa-0004", &body, &secret).await;
        assert_eq!(unknown.status, StatusCode::UNAUTHORIZED);
        // A forged delivery burnt no delivery id: the real one still lands.
        let late = k.deliver(APP1, "issues", "aaaa-0002", &body, &secret).await;
        assert_eq!(late.json()["handled"], "queued");

        let ping = k
            .deliver(APP1, "ping", "aaaa-0005", &json!({"zen": "x"}), &secret)
            .await;
        assert_eq!(ping.json()["handled"], "ping");
        let ignored = k
            .deliver(APP1, "star", "aaaa-0006", &json!({}), &secret)
            .await;
        assert_eq!(ignored.json()["handled"], "ignored");

        let huge = json!({"pad": "x".repeat(crate::MAX_WEBHOOK_BYTES)});
        let big = k.deliver(APP1, "issues", "aaaa-0007", &huge, &secret).await;
        assert_eq!(big.status, StatusCode::PAYLOAD_TOO_LARGE);

        // W2's app cannot queue work against W1's installation.
        let foreign = issue_event(INST1, REPO1, "EarthOS-world/EarthOS.world", 3);
        let other = k
            .deliver(APP2, "issues", "bbbb-0001", &foreign, &webhook_secret(APP2))
            .await;
        assert_eq!(other.json()["handled"], "unbound");
        assert_eq!(k.queue_len().await, 2);
        // The webhook never calls GitHub.
        k.gh.clear_log();
        let _ = k
            .deliver(
                APP1,
                "push",
                "aaaa-0008",
                &json!({
                    "ref": "refs/heads/main", "before": "a", "after": "b",
                    "repository": {"id": REPO1, "full_name": "EarthOS-world/EarthOS.world"},
                    "installation": {"id": INST1}, "commits": [],
                }),
                &secret,
            )
            .await;
        assert!(k.gh.log().is_empty(), "{:?}", k.gh.log());
        k.assert_no_secrets();
    });
}

#[test]
fn installation_lifecycle_webhooks_stop_and_resume_work() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        let secret = webhook_secret(APP1);
        let event = |action: &str| json!({"action": action, "installation": {"id": INST1, "account": {"login": "World-360", "type": "Organization"}}});
        k.deliver(
            APP1,
            "issues",
            "cccc-0001",
            &issue_event(INST1, REPO1, "EarthOS-world/EarthOS.world", 1),
            &secret,
        )
        .await;
        assert_eq!(k.queue_len().await, 1);

        k.deliver(
            APP1,
            "installation",
            "cccc-0002",
            &event("suspend"),
            &secret,
        )
        .await;
        let repo = crate::get_repo(k.db(), W1, INST1, REPO1)
            .await
            .unwrap()
            .unwrap();
        assert!(!repo.active);
        let queued = k
            .deliver(
                APP1,
                "issues",
                "cccc-0003",
                &issue_event(INST1, REPO1, "EarthOS-world/EarthOS.world", 1),
                &secret,
            )
            .await;
        assert_eq!(queued.json()["handled"], "inactive");
        // A suspended installation is not drained.
        k.tick(45).await;
        assert!(k.sources().await.iter().all(|s| !s.2.contains("EarthOS")));
        assert_eq!(
            k.queue_len().await,
            1,
            "a suspension keeps the queue for later"
        );

        k.deliver(
            APP1,
            "installation",
            "cccc-0004",
            &event("unsuspend"),
            &secret,
        )
        .await;
        assert!(
            crate::get_repo(k.db(), W1, INST1, REPO1)
                .await
                .unwrap()
                .unwrap()
                .active
        );

        k.deliver(APP1, "installation_repositories", "cccc-0005", &json!({
            "action": "added",
            "installation": {"id": INST1, "account": {"login": "World-360", "type": "Organization"}},
            "repositories_added": [{"id": 77_002, "full_name": "EarthOS-world/website"}],
            "repositories_removed": [{"id": REPO1, "full_name": "EarthOS-world/EarthOS.world"}],
        }), &secret).await;
        assert!(
            crate::get_repo(k.db(), W1, INST1, 77_002)
                .await
                .unwrap()
                .unwrap()
                .active
        );
        assert!(
            !crate::get_repo(k.db(), W1, INST1, REPO1)
                .await
                .unwrap()
                .unwrap()
                .active
        );

        k.deliver(
            APP1,
            "installation",
            "cccc-0006",
            &event("deleted"),
            &secret,
        )
        .await;
        let installs = crate::list_installations(k.db(), W1).await.unwrap();
        assert_eq!(installs[0].status, InstallationStatus::Deleted);
        assert_eq!(k.queue_len().await, 0, "deletion drops the queue");
    });
}

// ---------------------------------------------------------------------------
// The drain

#[test]
fn the_drain_backfills_within_its_budget_resumes_and_files_the_right_records() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        k.gh.clear_log();

        // A tick of 3 calls: the token, the commit listing, the issue listing.
        k.tick(3).await;
        assert_eq!(k.gh.log().len(), 3, "{:?}", k.gh.log());
        let repo = crate::get_repo(k.db(), W1, INST1, REPO1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repo.backfill_state, BackfillPhase::Running);
        let cursor = DrainCursor::parse(repo.backfill_cursor.as_deref());
        // Resumable: the commits are queued in the stored cursor, and no
        // GitHub content is.
        assert_eq!(cursor.state.commit_queue.len(), 3);
        assert!(!repo.backfill_cursor.unwrap().contains("the body"));

        // Enough ticks finish both workspaces' repositories.
        for _ in 0..6 {
            k.tick(45).await;
        }
        let repo = crate::get_repo(k.db(), W1, INST1, REPO1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(repo.backfill_state, BackfillPhase::Done, "{repo:?}");
        assert!(repo.last_error.is_none(), "{repo:?}");

        let sources = k.sources().await;
        let w1_scope = livingbrain_pages::page_scope(W1, &livingbrain_access::Scope::Shared);
        let w2_scope = livingbrain_pages::page_scope(W2, &livingbrain_access::Scope::Shared);
        let w1: Vec<_> = sources.iter().filter(|s| s.1 == w1_scope).collect();
        let w2: Vec<_> = sources.iter().filter(|s| s.1 == w2_scope).collect();
        assert_eq!(
            w1.len() + w2.len(),
            sources.len(),
            "nothing outside the two scopes"
        );
        let kinds = |rows: &[&(String, String, String, String)], kind: &str| {
            rows.iter().filter(|r| r.0 == kind).count()
        };
        assert_eq!(kinds(&w1, "github_commit"), 3);
        assert_eq!(kinds(&w1, "github_issue"), 2);
        assert_eq!(kinds(&w1, "github_pr"), 1);
        assert!(w1.iter().all(|r| r.3 == "EarthOS.world"), "{w1:?}");
        assert!(
            w1.iter()
                .any(|r| r.2 == "github/EarthOS-world/EarthOS.world/pr/3"),
            "{w1:?}"
        );
        assert_eq!(kinds(&w2, "github_commit"), 1);
        assert!(
            w2.iter()
                .all(|r| r.2.starts_with("github/Other-org/secret-plans/"))
        );

        // Every record was filed by the member who bound the installation.
        let importers = k
            .db()
            .query(&Statement::new(
                "SELECT DISTINCT imported_by FROM sources ORDER BY imported_by",
            ))
            .await
            .unwrap();
        let importers: Vec<String> = importers
            .rows
            .iter()
            .map(|r| get_text(r, "imported_by"))
            .collect();
        assert_eq!(importers, vec![OWNER.to_owned(), OWNER2.to_owned()]);

        // Done is quiet: a tick reads nothing.
        k.gh.clear_log();
        k.tick(45).await;
        assert!(k.gh.log().is_empty(), "{:?}", k.gh.log());

        // A push turns the incremental pass on; it reads only what is new,
        // and what it re-reads dedupes.
        let before = k.sources().await.len();
        k.gh.set_commits(REPO1, 5);
        k.deliver(
            APP1,
            "push",
            "dddd-0001",
            &json!({
                "ref": "refs/heads/main", "before": "a", "after": "b",
                "repository": {"id": REPO1, "full_name": "EarthOS-world/EarthOS.world"},
                "installation": {"id": INST1}, "commits": [],
            }),
            &webhook_secret(APP1),
        )
        .await;
        k.tick(45).await;
        k.tick(45).await;
        let after = k.sources().await;
        assert_eq!(
            after.len(),
            before + 2,
            "two new commits, nothing duplicated"
        );
        let details: Vec<String> =
            k.gh.log()
                .into_iter()
                .filter(|c| c.contains("/commits/"))
                .collect();
        assert_eq!(
            details.len(),
            2,
            "only the new commits' details: {details:?}"
        );
        k.assert_no_secrets();
    });
}

#[test]
fn a_rate_limit_backs_the_installation_off_and_stores_when_to_return() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        k.gh.clear_log();
        let reset = (NOW + 600).to_string();
        k.gh.script(
            "/repos/EarthOS-world/EarthOS.world/commits",
            403,
            &[
                ("x-ratelimit-remaining", "0"),
                ("x-ratelimit-reset", reset.as_str()),
            ],
        );
        k.tick(45).await;
        let repo = crate::get_repo(k.db(), W1, INST1, REPO1)
            .await
            .unwrap()
            .unwrap();
        let cursor = DrainCursor::parse(repo.backfill_cursor.as_deref());
        assert_eq!(cursor.wait_until, Some(NOW + 601));
        // Nothing more was asked of that installation in the same tick.
        let w1_calls: Vec<String> =
            k.gh.log()
                .into_iter()
                .filter(|c| c.contains("EarthOS"))
                .collect();
        assert_eq!(w1_calls.len(), 1, "{w1_calls:?}");
        // While waiting, the repository is skipped entirely.
        k.gh.clear_log();
        k.tick(45).await;
        assert!(
            k.gh.log().iter().all(|c| !c.contains("EarthOS")),
            "{:?}",
            k.gh.log()
        );

        // The settings page can say when it resumes.
        let summary = k.get("/v1/github", Some(&k.cookie(W1, OWNER))).await.json();
        assert_eq!(summary["repos"][0]["backfill"]["waiting_until"], NOW + 601);
    });
}

#[test]
fn a_permission_failure_is_recorded_and_backed_off() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        k.gh.script(
            "/repos/EarthOS-world/EarthOS.world/commits",
            403,
            &[("x-ratelimit-remaining", "4000")],
        );
        k.tick(45).await;
        let repo = crate::get_repo(k.db(), W1, INST1, REPO1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            repo.last_error.as_deref(),
            Some("the commit listing: GitHub answered 403")
        );
        let cursor = DrainCursor::parse(repo.backfill_cursor.as_deref());
        assert_eq!(cursor.failures, 1);
        assert_eq!(cursor.wait_until, Some(NOW + 300));
    });
}

#[test]
fn a_queued_item_is_refetched_and_filed_and_a_failing_one_dies_after_five_tries() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        k.deliver(
            APP1,
            "pull_request",
            "eeee-0001",
            &json!({
                "action": "opened", "number": 3, "pull_request": {"number": 3},
                "repository": {"id": REPO1, "full_name": "EarthOS-world/EarthOS.world"},
                "installation": {"id": INST1},
            }),
            &webhook_secret(APP1),
        )
        .await;
        k.gh.clear_log();
        // A budget for the queue item only; repositories get the rest.
        k.tick(45).await;
        assert!(
            k.gh.log().iter().any(|c| c.contains("/issues/3")),
            "{:?}",
            k.gh.log()
        );
        assert!(
            k.gh.log().iter().any(|c| c.contains("/pulls/3/reviews")),
            "{:?}",
            k.gh.log()
        );
        assert_eq!(k.queue_len().await, 0);
        let prs: Vec<_> = k
            .sources()
            .await
            .into_iter()
            .filter(|s| s.0 == "github_pr")
            .collect();
        assert_eq!(prs.len(), 1, "{prs:?}");

        // An item that keeps failing is parked dead after five attempts.
        k.deliver(
            APP1,
            "issues",
            "eeee-0002",
            &issue_event(INST1, REPO1, "EarthOS-world/EarthOS.world", 2),
            &webhook_secret(APP1),
        )
        .await;
        for _ in 0..5 {
            k.gh.script("/issues/2", 500, &[]);
            // The failed item's retry is in the future; make it due again.
            k.db()
                .execute(&Statement::new("UPDATE github_queue SET available_at = 0"))
                .await
                .unwrap();
            k.tick(45).await;
        }
        let dead = crate::dead_count(k.db(), W1).await.unwrap();
        assert_eq!(dead, 1);
    });
}

#[test]
fn the_drain_never_runs_outside_scheduled_and_settings_cross_no_workspace() {
    pollster::block_on(async {
        let k = kit();
        k.connected().await;
        // Every route, exercised: none of them reads repository history.
        let owner = k.cookie(W1, OWNER);
        let other = k.cookie(W2, OWNER2);
        let _ = k.get("/v1/github", Some(&owner)).await;
        let patched = k
            .write(
                Method::PATCH,
                &format!("/v1/github/repos/{REPO1}"),
                &owner,
                json!({"included": false}),
            )
            .await;
        assert_eq!(patched.status, StatusCode::OK);
        assert_eq!(patched.json()["included"], false);
        let restored = k
            .write(
                Method::PATCH,
                &format!("/v1/github/repos/{REPO1}"),
                &owner,
                json!({"included": true, "scope": format!("user:{OWNER}")}),
            )
            .await;
        assert_eq!(restored.json()["scope"], format!("user:{OWNER}"));
        // Another workspace's repository is not there for W2.
        let crossed = k
            .write(
                Method::PATCH,
                &format!("/v1/github/repos/{REPO1}"),
                &other,
                json!({"included": false}),
            )
            .await;
        assert_eq!(crossed.status, StatusCode::NOT_FOUND);
        let w2 = k.get("/v1/github", Some(&other)).await.json();
        assert_eq!(w2["repos"].as_array().unwrap().len(), 1);
        assert_eq!(w2["repos"][0]["repo_id"], REPO2);
        assert!(!w2.to_string().contains("EarthOS"));
        // A plain member reads but cannot write.
        let plain = k.cookie(W1, PLAIN);
        assert_eq!(
            k.get("/v1/github", Some(&plain)).await.json()["can_manage"],
            false
        );
        let refused = k
            .write(
                Method::PATCH,
                &format!("/v1/github/repos/{REPO1}"),
                &plain,
                json!({"included": false}),
            )
            .await;
        assert_eq!(refused.status, StatusCode::FORBIDDEN);
        assert!(
            k.gh.log().iter().all(|c| !c.contains("/repos/")),
            "{:?}",
            k.gh.log()
        );

        // The personal scope files into that member's scope.
        for _ in 0..4 {
            k.tick(45).await;
        }
        let personal = livingbrain_pages::page_scope(
            W1,
            &livingbrain_access::Scope::User(livingbrain_access::UserId::new(OWNER.to_owned())),
        );
        let sources = k.sources().await;
        assert!(
            sources
                .iter()
                .filter(|s| s.2.contains("EarthOS"))
                .all(|s| s.1 == personal),
            "{sources:?}"
        );

        // Removing the app forgets it and says where to finish on GitHub.
        let removed = k
            .write(Method::DELETE, "/v1/github/app", &owner, json!({}))
            .await;
        assert_eq!(removed.status, StatusCode::OK);
        let removed = removed.json();
        assert_eq!(removed["removed"], true);
        assert!(
            removed["next_step"]["url"]
                .as_str()
                .unwrap()
                .starts_with("https://github.com/apps/")
        );
        assert!(crate::app_summary(k.db(), W1).await.unwrap().is_none());
        assert!(crate::app_summary(k.db(), W2).await.unwrap().is_some());
        k.assert_no_secrets();
    });
}

#[test]
fn no_route_can_trigger_a_backfill() {
    // The user rule, checked against the route table: nothing under
    // /v1/github is named for running, syncing or importing, and the only
    // `sync::run` caller is the scheduled drain.
    let routes = include_str!("routes.rs");
    for word in ["/sync", "/run", "/import", "/backfill", "/refresh"] {
        assert!(!routes.contains(&format!("route(\"{word}")), "{word}");
    }
    let sources = ["routes.rs", "intake.rs", "github.rs", "lib.rs"];
    for file in sources {
        let body = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join(file),
        )
        .unwrap();
        assert!(!body.contains("sync::run"), "{file} reaches the drain body");
    }
    let drain = include_str!("drain.rs");
    assert_eq!(drain.matches("crate::sync::run(").count(), 1);
}
