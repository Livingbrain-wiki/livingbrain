//! The `apps` module: `/v1/apps`, where a workspace owner connects an app
//! (SupportGenius, EarthOS Aura) and issues, lists and revokes its
//! credential (issues #51 and #146; ADR 0005).
//!
//! | Route | What |
//! |---|---|
//! | `POST /` `{app}` | connect the app (once) and mint a credential; the key is in this answer only |
//! | `GET /` | the connected apps and their keys' public prefixes |
//! | `DELETE /{app}` | revoke: every key, the account, the grants |
//! | `DELETE /{app}/keys/{prefix}` | revoke one key |
//! | `GET /{app}/subjects/{subject}` | everything the app keeps for one subject (export) |
//! | `DELETE /{app}/subjects/{subject}` | erase it (the right to be forgotten, by the owner) |
//!
//! Every route is for a workspace **owner** with their whole credential
//! ([`Service::owner_only`]). A connected app is an **agent account**
//! (`livingbrain-accounts`, ADR 0004) named `app:<name>`, owned by the owner
//! who connected it, with five **tool grants** and nothing else: no page, no
//! source, no fact. Its key is an ordinary `lba_…` key, so revoking it, or
//! disabling the account, or removing its owner takes effect on the next
//! request.

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{delete, get};
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use cratefield_module_orgs::grants::{self, Effect, NewGrant, Permission, Selector};
use cratefield_module_orgs::service_accounts::{
    self as accounts, NewKey, NewServiceAccount, ServiceAccount,
};
use livingbrain_access::{Scope, UserId};
use livingbrain_pages::page_scope;
use serde::Deserialize;
use serde_json::{Value, json};
use time::Duration;

use crate::Wiki;
use crate::problems::{NOT_FOUND, TOOL_INPUT, page_read_error};
use crate::service::Service;

/// The five tools a connected app may call, in the order they are listed.
pub const TOOLS: [&str; 5] = [
    "brain_answer_public",
    "brain_memory_recall",
    "brain_memory_note",
    "brain_memory_forget",
    "brain_memory_forget_all",
];

/// The resource kind a tool grant names.
pub const TOOLS_KIND: &str = "tools";

/// The prefix of a connected app's agent account name.
const ACCOUNT_PREFIX: &str = "app:";

/// The grant space of an app: `app:<name>`.
#[must_use]
pub fn app_space(app: &str) -> String {
    format!("app:{app}")
}

/// Whether `app` is a name an app may be connected under: a lowercase letter
/// then lowercase letters, digits and `-`, 2 to 32 characters. No `:`, so
/// `<app>:<subject>` always splits at its first colon.
#[must_use]
pub fn is_app_name(app: &str) -> bool {
    (2..=32).contains(&app.len())
        && app.as_bytes()[0].is_ascii_lowercase()
        && app
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `subject` is one a client may name: 1 to 200 characters, no
/// control character, no surrounding whitespace.
#[must_use]
pub fn is_subject(subject: &str) -> bool {
    !subject.is_empty()
        && subject.chars().count() <= 200
        && subject == subject.trim()
        && !subject.chars().any(char::is_control)
}

/// The private page scope of `app`'s `subject` in `workspace_id`.
///
/// The subject maps to a [`Scope::User`] whose id is `<app>:<subject>` (ADR
/// 0005), so the workspace and the app are folded into the scope hash: the
/// same subject string under another app, or in another workspace, is a
/// different scope, and **nothing the client sends can name a scope outside
/// its own app's prefix**.
#[must_use]
pub fn subject_scope(workspace_id: &str, app: &str, subject: &str) -> String {
    page_scope(
        workspace_id,
        &Scope::User(UserId::new(format!("{app}:{subject}"))),
    )
}

/// The `apps` module.
pub struct Apps {
    wiki: Wiki,
}

impl Apps {
    /// An apps module over the given page access.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Apps {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    fn optional(&self) -> &'static [Port] {
        &[Port::Signer]
    }

    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let state = Arc::new(Service::from_wiki(ctx, &self.wiki));
        cratefield_core::axum::Router::new()
            .route("/", get(list).post(issue))
            .route("/{app}", delete(revoke))
            .route("/{app}/keys/{prefix}", delete(revoke_key))
            .route("/{app}/subjects/{subject}", get(export).delete(erase))
            .with_state(state)
    }
}

/// The account connected as `app`, if there is one.
async fn account_of(
    db: &dyn cratefield_core::Database,
    workspace_id: &str,
    app: &str,
) -> Result<Option<ServiceAccount>, Problem> {
    let name = format!("{ACCOUNT_PREFIX}{app}");
    Ok(accounts::list(db, workspace_id, None)
        .await
        .map_err(|_| Problem::internal())?
        .into_iter()
        .find(|account| account.name == name))
}

fn checked_app(app: &str) -> Result<(), Problem> {
    if is_app_name(app) {
        Ok(())
    } else {
        Err(Problem::new(&TOOL_INPUT).with_detail(
            "`app` must be 2-32 characters: a lowercase letter, then lowercase letters, \
             digits and `-`",
        ))
    }
}

#[derive(Debug, Deserialize)]
struct IssueRequest {
    app: String,
    /// The key stops working after this many days; none is for no expiry.
    #[serde(default)]
    expires_in_days: Option<u32>,
}

/// `POST /` — connects `app` if it is not connected yet, restores it if it
/// was revoked, and mints one credential.
///
/// The key is in this answer and nowhere else, ever: only its SHA-256 is
/// stored. Issuing again mints a second key for the same app (rotate by
/// issuing, then revoking the old prefix); it never changes the grants.
async fn issue(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Json(request): Json<IssueRequest>,
) -> Result<Response, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    checked_app(&request.app)?;
    let (db, clock, id_gen) = state.core()?;
    let now = clock.now();
    let org = asker.workspace_id.as_str();
    let account = match account_of(&*db, org, &request.app).await? {
        Some(account) => {
            if !account.is_enabled() {
                accounts::set_disabled(&*db, org, &account.id, false, now)
                    .await
                    .map_err(|_| Problem::internal())?;
            }
            account
        }
        None => accounts::create(
            &*db,
            org,
            &NewServiceAccount {
                id: &format!("agt_{}", id_gen.ulid()),
                name: &format!("{ACCOUNT_PREFIX}{}", request.app),
                description: &format!("Connected app: {}", request.app),
                owner_sub: &asker.user_id,
                created_by: &asker.user_id,
            },
            now,
        )
        .await
        .map_err(|_| Problem::internal())?,
    };
    let principal = livingbrain_pages::access::agent_principal(&account.id);
    for tool in TOOLS {
        let selector = Selector::new(TOOLS_KIND, app_space(&request.app), vec![tool.to_owned()])
            .map_err(|_| Problem::internal())?;
        grants::put_grant(
            &*db,
            org,
            &format!("grt_{}", id_gen.ulid()),
            &NewGrant {
                principal: principal.clone(),
                selector,
                permission: Permission::Write,
                effect: Effect::Allow,
                expires_at: None,
                created_by: asker.user_id.clone(),
            },
            now,
        )
        .await
        .map_err(|_| Problem::internal())?;
    }
    let expires_at = request
        .expires_in_days
        .map(|days| now + Duration::days(i64::from(days.clamp(1, 3650))));
    let minted = accounts::mint_key(
        &*db,
        org,
        &account.id,
        &NewKey {
            brand: livingbrain_tokens::AGENT_KEY_BRAND,
            name: &format!("{} credential", request.app),
            scopes: None,
            expires_at,
        },
        now,
    )
    .await
    .map_err(|_| Problem::internal())?;
    state
        .audit(&asker, "app.issue", &format!("app:{}", request.app))
        .await;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "app": request.app,
            "agent_id": account.id,
            "prefix": minted.info.prefix,
            "key": minted.key,
            "expires_at": minted.info.expires_at,
            "tools": TOOLS,
            "note": "This key is shown once. Store it as a secret; revoke it with \
                     DELETE /v1/apps/{app}/keys/{prefix}.",
        })),
    )
        .into_response())
}

/// `GET /` — the connected apps, never a secret.
async fn list(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    let (db, _, _) = state.core()?;
    let org = asker.workspace_id.as_str();
    let mut apps = Vec::new();
    for account in accounts::list(&*db, org, None)
        .await
        .map_err(|_| Problem::internal())?
    {
        let Some(app) = account.name.strip_prefix(ACCOUNT_PREFIX) else {
            continue;
        };
        let keys = accounts::keys(&*db, org, &account.id)
            .await
            .map_err(|_| Problem::internal())?;
        apps.push(json!({
            "app": app,
            "agent_id": account.id,
            "disabled": account.disabled_at.is_some(),
            "created_at": account.created_at,
            "keys": keys.iter().map(|key| json!({
                "prefix": key.prefix,
                "created_at": key.created_at,
                "expires_at": key.expires_at,
                "revoked_at": key.revoked_at,
                "last_used_at": key.last_used_at,
            })).collect::<Vec<_>>(),
        }));
    }
    Ok(Json(json!({ "apps": apps })))
}

/// `DELETE /{app}` — revokes every key, disables the account and deletes its
/// grants, so the next request with any of its keys is a 401. The subjects'
/// memories are not touched (they are the members'; erase them with
/// `DELETE /{app}/subjects/{subject}`).
async fn revoke(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path(app): Path<String>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    checked_app(&app)?;
    let (db, clock, _) = state.core()?;
    let now = clock.now();
    let org = asker.workspace_id.as_str();
    let account = account_of(&*db, org, &app)
        .await?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    let mut revoked = 0_u32;
    for key in accounts::keys(&*db, org, &account.id)
        .await
        .map_err(|_| Problem::internal())?
    {
        if accounts::revoke_key(&*db, org, &account.id, &key.prefix, now)
            .await
            .map_err(|_| Problem::internal())?
        {
            revoked += 1;
        }
    }
    accounts::set_disabled(&*db, org, &account.id, true, now)
        .await
        .map_err(|_| Problem::internal())?;
    grants::delete_grants_for(
        &*db,
        org,
        &livingbrain_pages::access::agent_principal(&account.id),
    )
    .await
    .map_err(|_| Problem::internal())?;
    state
        .audit(&asker, "app.revoke", &format!("app:{app}"))
        .await;
    Ok(Json(json!({ "app": app, "revoked_keys": revoked })))
}

/// `DELETE /{app}/keys/{prefix}` — revokes one key.
async fn revoke_key(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path((app, prefix)): Path<(String, String)>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    checked_app(&app)?;
    let (db, clock, _) = state.core()?;
    let org = asker.workspace_id.as_str();
    let account = account_of(&*db, org, &app)
        .await?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    let revoked = accounts::revoke_key(&*db, org, &account.id, &prefix, clock.now())
        .await
        .map_err(|_| Problem::internal())?;
    if !revoked {
        return Err(Problem::new(&NOT_FOUND));
    }
    state
        .audit(&asker, "app.key.revoke", &format!("app:{app}/{prefix}"))
        .await;
    Ok(Json(json!({ "app": app, "revoked": prefix })))
}

fn checked_subject(subject: &str) -> Result<(), Problem> {
    if is_subject(subject) {
        Ok(())
    } else {
        Err(Problem::new(&TOOL_INPUT).with_detail(
            "`subject` must be 1-200 characters, with no control character and no \
             surrounding whitespace",
        ))
    }
}

/// `GET /{app}/subjects/{subject}` — everything the app keeps for the
/// subject, in full: the export half of a data-subject request.
async fn export(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path((app, subject)): Path<(String, String)>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    checked_app(&app)?;
    checked_subject(&subject)?;
    let scope = subject_scope(&asker.workspace_id, &app, &subject);
    let memories = state
        .memories_store()?
        .recall(&scope, livingbrain_pages::MAX_MEMORIES)
        .await
        .map_err(page_read_error)?;
    state
        .audit(
            &asker,
            "app.subject.export",
            &format!("app:{app}/{subject}"),
        )
        .await;
    Ok(Json(json!({
        "app": app,
        "subject": subject,
        "memories": memories.iter().map(|memory| json!({
            "id": memory.id,
            "text": memory.text,
            "source": memory.source,
            "created_at": memory.created_at,
        })).collect::<Vec<_>>(),
    })))
}

/// `DELETE /{app}/subjects/{subject}` — erases everything the app keeps for
/// the subject.
async fn erase(
    State(state): State<Arc<Service>>,
    method: Method,
    headers: HeaderMap,
    Path((app, subject)): Path<(String, String)>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&method, &headers).await?;
    Service::owner_only(&asker)?;
    checked_app(&app)?;
    checked_subject(&subject)?;
    let scope = subject_scope(&asker.workspace_id, &app, &subject);
    let forgotten = state
        .memories_store()?
        .forget_all(&scope)
        .await
        .map_err(page_read_error)?;
    state
        .audit(&asker, "app.subject.erase", &format!("app:{app}/{subject}"))
        .await;
    Ok(Json(json!({ "forgotten": forgotten })))
}
