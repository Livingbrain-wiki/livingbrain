//! The routes under `/v1/github`.
//!
//! | route | who | what |
//! |---|---|---|
//! | `GET /` | any member | the app (no secrets), installations, repositories with backfill progress, queue counts |
//! | `POST /app/manifest` | owner/admin | mints a manifest state; answers the GitHub form target and the manifest JSON |
//! | `GET /app/callback?code&state` | the member who started it | converts the manifest, seals the credentials, redirects to the install page |
//! | `POST /app/install` | owner/admin | mints an install state; answers the install URL (to add another account) |
//! | `GET /install/callback?installation_id&setup_action&state` | the member who started it | confirms the installation **with GitHub**, binds it, lists its repositories |
//! | `PATCH /repos/{id}` | owner/admin | include/exclude, scope, project |
//! | `DELETE /app` | owner/admin | forgets the app and its credentials |
//! | `POST /webhook` (and `/webhook/{app_id}`) | GitHub, signed | see [`crate::intake`] |
//!
//! A cookie-authenticated write must be same-origin (the tokens module's
//! one rule); a bearer write never needs it. The two callbacks are `GET`
//! navigations from github.com, so they carry the session cookie
//! (`SameSite=Lax`) and are guarded by their single-use state, which is
//! bound to the member who minted it. They answer with a redirect to the
//! settings page — always a fixed same-origin path, never a URL read from
//! the request.
//!
//! **There is no route that runs a backfill or an import.** Including a
//! repository makes it due; the next cron tick picks it up.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{delete, get, patch, post};
use cratefield_core::axum::{self, Json, body::Bytes};
use cratefield_core::{Clock, Database, HttpClient, ModuleContext, Problem, ProblemDef, Statement};
use cratefield_kms::Kms;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::common::{StoreError, get_text, text, unix};
use crate::github::{self, GhError};
use crate::{
    Account, BindOutcome, DEFAULT_STATE_TTL_SECS, InstallationStatus, NewApp, Repo, RepoRef,
    RepoSettings, SaveAppOutcome, Shared, StatePurpose,
};

pub(crate) const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "github/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Owner or admin access required",
    description: "Only a workspace owner or admin can connect or change GitHub.",
};

pub(crate) const CROSS_SITE: ProblemDef = ProblemDef {
    slug: "github/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "Cross-site request refused",
    description: "A cookie-authenticated write must come from this origin.",
};

pub(crate) const NOT_CONFIGURED: ProblemDef = ProblemDef {
    slug: "github/not-configured",
    status: StatusCode::SERVICE_UNAVAILABLE,
    title: "GitHub is not configured on this deployment",
    description: "The key ring the GitHub App credentials are sealed under is missing.",
};

pub(crate) const APP_EXISTS: ProblemDef = ProblemDef {
    slug: "github/app-exists",
    status: StatusCode::CONFLICT,
    title: "This workspace already has a GitHub App",
    description: "Remove the existing app before creating another.",
};

pub(crate) const NO_APP: ProblemDef = ProblemDef {
    slug: "github/no-app",
    status: StatusCode::CONFLICT,
    title: "This workspace has no GitHub App yet",
    description: "Create the app with the manifest flow first.",
};

pub(crate) const REPO_NOT_FOUND: ProblemDef = ProblemDef {
    slug: "github/repo-not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such repository in this workspace",
    description: "The repository id names nothing this workspace's installations reach.",
};

pub(crate) struct RouteState {
    pub ctx: ModuleContext,
    pub shared: Arc<Shared>,
}

pub(crate) type St = State<Arc<RouteState>>;

pub(crate) fn router(ctx: ModuleContext, shared: Arc<Shared>) -> axum::Router {
    axum::Router::new()
        .route("/", get(summary))
        .route("/app", delete(remove_app))
        .route("/app/manifest", post(manifest))
        .route("/app/callback", get(app_callback))
        .route("/app/install", post(start_install))
        .route("/install/callback", get(install_callback))
        .route("/repos/{id}", patch(update_repo))
        .route("/webhook", post(crate::intake::webhook))
        .route("/webhook/{app_id}", post(crate::intake::webhook_for_app))
        .with_state(Arc::new(RouteState { ctx, shared }))
}

// ---------------------------------------------------------------------------
// Ports and errors

pub(crate) fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

pub(crate) fn kms(state: &RouteState) -> Result<Arc<dyn Kms>, Problem> {
    state
        .shared
        .kms
        .clone()
        .ok_or_else(|| Problem::new(&NOT_CONFIGURED))
}

/// A store failure as a response. `Forbidden` and `Invalid` are the
/// caller's; anything else is ours and says nothing about why.
pub(crate) fn store(error: StoreError) -> Problem {
    match error {
        StoreError::Forbidden => Problem::new(&FORBIDDEN),
        StoreError::Invalid(what) => Problem::validation_failed(format!("invalid {what}")),
        StoreError::Store(_) | StoreError::Kms(_) => Problem::internal(),
    }
}

struct Ports {
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
    http: Arc<dyn HttpClient>,
}

fn ports(state: &RouteState) -> Result<Ports, Problem> {
    Ok(Ports {
        db: port(state.ctx.ports.db.clone())?,
        clock: port(state.ctx.ports.clock.clone())?,
        http: port(state.ctx.ports.http.clone())?,
    })
}

/// The caller, by bearer or by session cookie; a cookie write must be
/// same-origin.
async fn caller(
    state: &RouteState,
    headers: &HeaderMap,
    write: bool,
) -> Result<livingbrain_tokens::Member, Problem> {
    if write && cratefield_core::bearer_token(headers).is_none() {
        livingbrain_tokens::same_origin(headers)
            .map_err(|refused| Problem::new(&CROSS_SITE).with_detail(refused.detail))?;
    }
    Ok(livingbrain_tokens::authenticate(&state.ctx.ports, headers).await?)
}

async fn admin(
    state: &RouteState,
    headers: &HeaderMap,
) -> Result<livingbrain_tokens::Member, Problem> {
    let member = caller(state, headers, true).await?;
    if member.is_admin {
        Ok(member)
    } else {
        Err(Problem::new(&FORBIDDEN))
    }
}

/// The web app's origin (`WORKSPACES_REDIRECT_BASE`): where GitHub is told
/// to send the browser and the webhooks.
fn base(state: &RouteState) -> String {
    livingbrain_pages::app_base(state.ctx.config.as_ref())
}

/// The settings page, with the outcome of a callback. A fixed path on this
/// origin: nothing from the request is ever put in a redirect target.
fn settings_redirect(outcome: &str, reason: Option<&str>) -> Response {
    let mut target = format!("/settings.html?github={outcome}");
    if let Some(reason) = reason {
        target.push_str("&reason=");
        target.push_str(reason);
    }
    target.push_str("#github");
    redirect(&target)
}

fn redirect(target: &str) -> Response {
    match HeaderValue::from_str(target) {
        Ok(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
        Err(_) => Problem::internal().into_response(),
    }
}

/// Reasons a callback sends back to the settings page.
fn failed(reason: &str) -> Response {
    settings_redirect("error", Some(reason))
}

// ---------------------------------------------------------------------------
// Settings reads

/// `GET /v1/github` — everything the settings page shows, no secrets.
async fn summary(State(state): St, headers: HeaderMap) -> Result<Json<Value>, Problem> {
    let member = caller(&state, &headers, false).await?;
    let db = port(state.ctx.ports.db.clone())?;
    let ws = member.workspace_id.as_str();
    let app = crate::app_summary(&*db, ws).await.map_err(store)?;
    let installations = crate::list_installations(&*db, ws).await.map_err(store)?;
    let repos = crate::list_repos(&*db, ws).await.map_err(store)?;
    let queued = crate::queued_count(&*db, ws).await.map_err(store)?;
    let dead = crate::dead_count(&*db, ws).await.map_err(store)?;
    Ok(Json(json!({
        "configured": state.shared.kms.is_some(),
        "can_manage": member.is_admin,
        "app": app.map(|app| json!({
            "app_id": app.app_id,
            "slug": app.slug,
            "client_id": app.client_id,
            "created_by": app.created_by,
            "created_at": app.created_at,
            "html_url": format!("{}/apps/{}", github::WEB, app.slug),
            "install_url": format!("{}/apps/{}/installations/new", github::WEB, app.slug),
        })),
        "installations": installations.iter().map(|i| json!({
            "installation_id": i.installation_id,
            "account": {"login": i.account.login, "type": i.account.kind},
            "status": status_name(i.status),
            "bound_by": i.bound_by,
            "bound_at": i.bound_at,
        })).collect::<Vec<_>>(),
        "repos": repos.iter().map(repo_json).collect::<Vec<_>>(),
        "queue": {"queued": queued, "dead": dead},
    })))
}

fn status_name(status: InstallationStatus) -> &'static str {
    match status {
        InstallationStatus::Active => "active",
        InstallationStatus::Suspended => "suspended",
        InstallationStatus::Deleted => "deleted",
    }
}

/// A repository as the settings page reads it. The backfill cursor is the
/// drain's own and is summarised, not echoed.
pub(crate) fn repo_json(repo: &Repo) -> Value {
    json!({
        "repo_id": repo.repo_id,
        "installation_id": repo.installation_id,
        "full_name": repo.full_name,
        "project": repo.project,
        "scope": repo.scope,
        "included": repo.included,
        "active": repo.active,
        "removed_at": repo.removed_at,
        "backfill": {
            "state": repo.backfill_state.as_str(),
            "items": repo.backfill_items,
            "batches": repo.backfill_batches,
            "started_at": repo.backfill_started_at,
            "finished_at": repo.backfill_finished_at,
            "waiting_until": crate::sync::waiting_until(repo.backfill_cursor.as_deref()),
        },
        "last_webhook_at": repo.last_webhook_at,
        "last_error": repo.last_error,
        "last_error_at": repo.last_error_at,
    })
}

// ---------------------------------------------------------------------------
// Manifest flow

#[derive(Deserialize, Default)]
struct ManifestBody {
    /// `user` (the default) or `org`.
    owner: Option<String>,
    /// The organization login, for `owner: "org"`.
    org: Option<String>,
}

/// A GitHub organization login: alphanumerics and single hyphens, 1..=39.
fn is_org_login(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// GitHub's longest app name.
const MAX_APP_NAME: usize = 34;

/// `Living Brain (<workspace>) <4 hex>` within GitHub's 34 characters: the
/// suffix keeps two workspaces of the same name (or a re-created app)
/// from colliding on GitHub's global name rule.
fn app_name(workspace_name: &str, suffix: &str) -> String {
    let fixed = "Living Brain () ".len() + suffix.len();
    let room = MAX_APP_NAME.saturating_sub(fixed);
    let clean: String = workspace_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(room)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        format!("Living Brain {suffix}")
    } else {
        format!("Living Brain ({clean}) {suffix}")
    }
}

async fn workspace_name(db: &dyn Database, workspace_id: &str) -> Result<String, Problem> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT name FROM workspaces WHERE id = ?",
            vec![text(workspace_id)],
        ))
        .await
        .map_err(|_| Problem::internal())?;
    Ok(rows
        .first()
        .map(|row| get_text(row, "name"))
        .unwrap_or_default())
}

/// The manifest GitHub creates the app from. Pure, so a test pins it.
pub(crate) fn manifest_json(base: &str, name: &str) -> Value {
    json!({
        "name": name,
        "url": base,
        "description": "Living Brain files this workspace's commits, issues and pull requests into its memory.",
        "hook_attributes": {"url": format!("{base}/v1/github/webhook"), "active": true},
        "redirect_url": format!("{base}/v1/github/app/callback"),
        "setup_url": format!("{base}/v1/github/install/callback"),
        "setup_on_update": false,
        "public": false,
        "request_oauth_on_install": false,
        "default_permissions": {
            "metadata": "read",
            "contents": "read",
            "issues": "read",
            "pull_requests": "read",
        },
        "default_events": [
            "push",
            "issues",
            "issue_comment",
            "pull_request",
            "pull_request_review",
            "pull_request_review_comment",
            "release",
        ],
    })
}

/// `POST /v1/github/app/manifest` — `{"owner": "user"}` or
/// `{"owner": "org", "org": "<login>"}`. Answers the form the browser
/// posts to GitHub: `action` (with the state), `field` and `manifest`.
async fn manifest(State(state): St, headers: HeaderMap, body: Bytes) -> Result<Response, Problem> {
    let member = admin(&state, &headers).await?;
    kms(&state)?;
    let p = ports(&state)?;
    let body: ManifestBody = if body.is_empty() {
        ManifestBody::default()
    } else {
        serde_json::from_slice(&body)
            .map_err(|_| Problem::validation_failed("the body is not the expected JSON"))?
    };
    let ws = member.workspace_id.as_str();
    if crate::app_summary(&*p.db, ws)
        .await
        .map_err(store)?
        .is_some()
    {
        return Err(Problem::new(&APP_EXISTS));
    }
    let target = match body.owner.as_deref().unwrap_or("user") {
        "user" => format!("{}/settings/apps/new", github::WEB),
        "org" => {
            let org = body.org.as_deref().unwrap_or_default();
            if !is_org_login(org) {
                return Err(Problem::validation_failed(
                    "org must be a GitHub organization login",
                ));
            }
            format!("{}/organizations/{org}/settings/apps/new", github::WEB)
        }
        _ => return Err(Problem::validation_failed("owner is `user` or `org`")),
    };
    let token = crate::create_state(
        &*p.db,
        p.clock.now(),
        StatePurpose::Manifest,
        ws,
        &member.user_id,
        DEFAULT_STATE_TTL_SECS,
    )
    .await
    .map_err(store)?;
    let suffix = crate::common::random_hex(2).map_err(store)?;
    let name = app_name(&workspace_name(&*p.db, ws).await?, &suffix);
    let manifest = manifest_json(&base(&state), &name);
    Ok(Json(json!({
        "action": format!("{target}?state={token}"),
        "method": "POST",
        "field": "manifest",
        "manifest": manifest,
        "manifest_json": manifest.to_string(),
        "expires_in": DEFAULT_STATE_TTL_SECS,
    }))
    .into_response())
}

/// A manifest `code`: the plain token GitHub hands back. Checked before it
/// is put in a URL path.
fn is_code(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// An app slug as GitHub mints one.
fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// `GET /v1/github/app/callback?code&state` — GitHub's redirect after the
/// app was created.
async fn app_callback(
    State(state): St,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let (Some(code), Some(token)) = (query.get("code"), query.get("state")) else {
        return failed("bad-request");
    };
    if !is_code(code) {
        return failed("bad-request");
    }
    let Ok(member) = livingbrain_tokens::authenticate(&state.ctx.ports, &headers).await else {
        return failed("signed-out");
    };
    let (Ok(kms), Ok(p)) = (kms(&state), ports(&state)) else {
        return failed("not-configured");
    };
    let ws = member.workspace_id.as_str();
    let now = p.clock.now();
    let proof = match crate::consume_state(
        &*p.db,
        now,
        StatePurpose::Manifest,
        token,
        ws,
        &member.user_id,
    )
    .await
    {
        Ok(Some(proof)) => proof,
        Ok(None) => return failed("state"),
        Err(_) => return failed("store"),
    };
    let Ok(conversion) = github::convert_manifest(&*p.http, code).await else {
        return failed("conversion");
    };
    if !is_slug(&conversion.slug) {
        return failed("conversion");
    }
    let new_app = NewApp {
        app_id: conversion.app_id,
        slug: conversion.slug.clone(),
        client_id: conversion.client_id,
        private_key_pem: conversion.pem,
        webhook_secret: conversion.webhook_secret,
        client_secret: conversion.client_secret,
    };
    match crate::save_app(&*kms, &*p.db, now, proof, &new_app).await {
        Ok(SaveAppOutcome::Saved) => {}
        Ok(SaveAppOutcome::WorkspaceHasApp) => return failed("app-exists"),
        Ok(SaveAppOutcome::AppIdTaken) => return failed("app-id-taken"),
        Err(StoreError::Forbidden) => return failed("forbidden"),
        Err(_) => return failed("store"),
    }
    match crate::create_state(
        &*p.db,
        now,
        StatePurpose::Install,
        ws,
        &member.user_id,
        DEFAULT_STATE_TTL_SECS,
    )
    .await
    {
        Ok(install) => redirect(&format!(
            "{}/apps/{}/installations/new?state={install}",
            github::WEB,
            conversion.slug
        )),
        Err(_) => settings_redirect("created", None),
    }
}

/// `POST /v1/github/app/install` — an install link for this workspace's
/// app (to install it on another account, or to finish an install that
/// was abandoned).
async fn start_install(State(state): St, headers: HeaderMap) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers).await?;
    kms(&state)?;
    let p = ports(&state)?;
    let ws = member.workspace_id.as_str();
    let Some(app) = crate::app_summary(&*p.db, ws).await.map_err(store)? else {
        return Err(Problem::new(&NO_APP));
    };
    let token = crate::create_state(
        &*p.db,
        p.clock.now(),
        StatePurpose::Install,
        ws,
        &member.user_id,
        DEFAULT_STATE_TTL_SECS,
    )
    .await
    .map_err(store)?;
    Ok(Json(json!({
        "url": format!("{}/apps/{}/installations/new?state={token}", github::WEB, app.slug),
        "expires_in": DEFAULT_STATE_TTL_SECS,
    })))
}

/// The longest repository list read at install time: 10 pages of 100.
const INSTALL_REPO_PAGES: u32 = 10;

/// `GET /v1/github/install/callback?installation_id&setup_action&state` —
/// GitHub's setup URL after the app was installed.
///
/// The `installation_id` is attacker-controlled text, so it is bound only
/// after GitHub itself confirms — with this workspace's app JWT — that it
/// is an installation of this app, and with the account GitHub names.
async fn install_callback(
    State(state): St,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if query.get("setup_action").map(String::as_str) == Some("request") {
        // An organization member asked an owner to approve the install;
        // the approval arrives later as an `installation` webhook.
        return settings_redirect("requested", None);
    }
    let installation_id = query
        .get("installation_id")
        .and_then(|raw| raw.parse::<i64>().ok())
        .filter(|id| *id > 0);
    let (Some(installation_id), Some(token)) = (installation_id, query.get("state")) else {
        return failed("bad-request");
    };
    let Ok(member) = livingbrain_tokens::authenticate(&state.ctx.ports, &headers).await else {
        return failed("signed-out");
    };
    let (Ok(kms), Ok(p)) = (kms(&state), ports(&state)) else {
        return failed("not-configured");
    };
    let ws = member.workspace_id.as_str();
    let now = p.clock.now();
    let proof = match crate::consume_state(
        &*p.db,
        now,
        StatePurpose::Install,
        token,
        ws,
        &member.user_id,
    )
    .await
    {
        Ok(Some(proof)) => proof,
        Ok(None) => return failed("state"),
        Err(_) => return failed("store"),
    };
    let creds = match crate::app_credentials(&*kms, &*p.db, ws).await {
        Ok(Some(creds)) => creds,
        Ok(None) => return failed("no-app"),
        Err(_) => return failed("store"),
    };
    let Ok(jwt) = github::app_jwt(&creds.private_key_pem, creds.app_id, unix(now)) else {
        return failed("github");
    };
    let confirmed =
        match github::confirm_installation(&*p.http, &jwt, creds.app_id, installation_id).await {
            Ok(Some(confirmed)) => confirmed,
            Ok(None) => return failed("not-confirmed"),
            Err(_) => return failed("github"),
        };
    let account = confirmed.account.as_ref().map_or_else(
        || Account {
            login: String::new(),
            kind: String::new(),
        },
        |account| Account {
            login: account.login.clone(),
            kind: account.kind.clone(),
        },
    );
    match crate::bind_installation(&*p.db, now, proof, installation_id, &account).await {
        Ok(BindOutcome::Bound | BindOutcome::AlreadyThisWorkspace) => {}
        Ok(BindOutcome::BoundElsewhere) => return failed("bound-elsewhere"),
        Ok(BindOutcome::Refused) => return failed("refused"),
        Ok(BindOutcome::NoApp) => return failed("no-app"),
        Err(StoreError::Forbidden) => return failed("forbidden"),
        Err(_) => return failed("store"),
    }
    if confirmed.suspended_at.is_some()
        && crate::set_installation_status(
            &*p.db,
            now,
            creds.app_id,
            installation_id,
            InstallationStatus::Suspended,
        )
        .await
        .is_err()
    {
        return failed("store");
    }
    if confirmed.suspended_at.is_some() {
        return settings_redirect("installed", Some("suspended"));
    }
    match list_and_record_repos(&state, &p, &creds, ws, installation_id).await {
        Ok(()) => settings_redirect("installed", None),
        Err(_) => settings_redirect("installed", Some("repos")),
    }
}

async fn list_and_record_repos(
    state: &RouteState,
    p: &Ports,
    creds: &crate::AppCredentials,
    ws: &str,
    installation_id: i64,
) -> Result<(), GhError> {
    let now = p.clock.now();
    let token = github::installation_token(
        &*p.http,
        &state.shared.tokens,
        &creds.private_key_pem,
        creds.app_id,
        installation_id,
        unix(now),
    )
    .await?;
    let repos = github::installation_repositories(&*p.http, &token, INSTALL_REPO_PAGES).await?;
    let refs: Vec<RepoRef> = repos
        .into_iter()
        .filter(|repo| repo.id > 0)
        .map(|repo| RepoRef {
            repo_id: repo.id,
            full_name: repo.full_name,
        })
        .collect();
    crate::upsert_repos(&*p.db, now, ws, installation_id, &refs)
        .await
        .map_err(|_| GhError::Shape)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings writes

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoPatch {
    included: Option<bool>,
    scope: Option<String>,
    project: Option<String>,
}

/// `PATCH /v1/github/repos/{repo_id}` — `{"included": bool, "scope":
/// "shared" | "user:<member id>", "project": "<name>"}`, any subset.
/// Including a repository makes it due for the next cron tick; nothing here
/// fetches anything.
async fn update_repo(
    State(state): St,
    headers: HeaderMap,
    Path(repo_id): Path<i64>,
    body: Bytes,
) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers).await?;
    let p = ports(&state)?;
    let patch: RepoPatch = serde_json::from_slice(&body)
        .map_err(|_| Problem::validation_failed("the body is not the expected JSON"))?;
    if let Some(project) = patch.project.as_deref()
        && (project.is_empty() || project.len() > 64)
    {
        return Err(Problem::validation_failed("project is 1..=64 characters"));
    }
    let ws = member.workspace_id.as_str();
    let repos = crate::list_repos(&*p.db, ws).await.map_err(store)?;
    let Some(repo) = repos.iter().find(|repo| repo.repo_id == repo_id) else {
        return Err(Problem::new(&REPO_NOT_FOUND));
    };
    let settings = RepoSettings {
        project: patch.project,
        scope: patch.scope,
        included: patch.included,
    };
    let found = crate::set_repo_settings(
        &*p.db,
        p.clock.now(),
        ws,
        &member.user_id,
        repo.installation_id,
        repo.repo_id,
        &settings,
    )
    .await
    .map_err(store)?;
    if !found {
        return Err(Problem::new(&REPO_NOT_FOUND));
    }
    let updated = crate::get_repo(&*p.db, ws, repo.installation_id, repo.repo_id)
        .await
        .map_err(store)?
        .ok_or_else(|| Problem::new(&REPO_NOT_FOUND))?;
    Ok(Json(repo_json(&updated)))
}

/// `DELETE /v1/github/app` — forgets the app: its sealed credentials go,
/// its installations are marked deleted, its repositories stop and its
/// queue is dropped. The app itself still exists on GitHub; the answer
/// says where to delete it.
async fn remove_app(State(state): St, headers: HeaderMap) -> Result<Json<Value>, Problem> {
    let member = admin(&state, &headers).await?;
    let p = ports(&state)?;
    let ws = member.workspace_id.as_str();
    let app = crate::app_summary(&*p.db, ws).await.map_err(store)?;
    let removed = crate::remove_app(&*p.db, p.clock.now(), ws, &member.user_id)
        .await
        .map_err(store)?;
    if let Some(app) = &app {
        state.shared.tokens.forget_app(app.app_id);
    }
    Ok(Json(json!({
        "removed": removed,
        "next_step": app.map(|app| json!({
            "message": "Living Brain no longer holds this app's credentials. Delete the app on GitHub too: open its settings, then Advanced, then Delete GitHub App.",
            "url": format!("{}/apps/{}", github::WEB, app.slug),
        })),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_name_fits_githubs_limit_and_keeps_the_workspace() {
        let name = app_name("World-360", "a1b2");
        assert_eq!(name, "Living Brain (World-360) a1b2");
        let long = app_name("An extremely long workspace name indeed", "a1b2");
        assert!(long.len() <= MAX_APP_NAME, "{long}");
        assert!(long.starts_with("Living Brain (An extreme"));
        assert_eq!(app_name("<script>", "ffff"), "Living Brain (script) ffff");
        assert_eq!(app_name("", "ffff"), "Living Brain ffff");
    }

    #[test]
    fn the_manifest_names_the_webhook_and_the_two_callbacks() {
        let manifest = manifest_json("https://app.livingbrain.wiki", "Living Brain (x) 0000");
        assert_eq!(
            manifest["hook_attributes"]["url"],
            "https://app.livingbrain.wiki/v1/github/webhook"
        );
        assert_eq!(
            manifest["redirect_url"],
            "https://app.livingbrain.wiki/v1/github/app/callback"
        );
        assert_eq!(
            manifest["setup_url"],
            "https://app.livingbrain.wiki/v1/github/install/callback"
        );
        assert_eq!(manifest["public"], false);
        assert_eq!(manifest["default_permissions"]["contents"], "read");
        assert_eq!(manifest["default_events"].as_array().map(Vec::len), Some(7));
        // Read-only, and nothing more: no write permission is ever asked for.
        for (_, level) in manifest["default_permissions"].as_object().unwrap() {
            assert_eq!(level, "read");
        }
    }

    #[test]
    fn codes_slugs_and_org_logins_are_checked_before_they_reach_a_url() {
        assert!(is_code("a1B2_c-3"));
        assert!(!is_code("../x"));
        assert!(!is_code("a?b"));
        assert!(!is_code(""));
        assert!(is_slug("living-brain-world-360"));
        assert!(!is_slug("Living"));
        assert!(!is_slug("a/b"));
        assert!(is_org_login("World-360"));
        assert!(!is_org_login("-x"));
        assert!(!is_org_login("a/b"));
        assert!(!is_org_login(&"a".repeat(40)));
    }
}
