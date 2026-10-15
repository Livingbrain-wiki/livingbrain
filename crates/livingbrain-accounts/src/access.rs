//! `/v1/access` (ADR 0004): the grants, and who can see a page.
//!
//! | Route | Who |
//! |---|---|
//! | `GET /grants` | an admin sees every grant; a member sees the grants on their own space and the ones naming them, their role or their agents |
//! | `POST /grants` | whoever holds `admin` on the selector: a member on their own space, an owner or admin on the shared one, or somebody a grant made an admin there |
//! | `DELETE /grants/{id}` | the same rule, on the grant's own selector |
//! | `GET /pages/{slug}` | anyone who can read the page |
//!
//! No workspace admin can grant themselves, or anybody, into another
//! member's personal space: only that member (or somebody they made an admin
//! of it) can.
//!
//! This module also applies the tables the three modules share — the
//! harness's service accounts, grants and audit log — under its own name.

use std::collections::BTreeSet;
use std::sync::Arc;

use cratefield_core::axum::extract::{Path, State as AxumState};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, IdGen, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem,
    SqlMigration,
};
use cratefield_module_orgs::grants::{
    self, ANY_KIND, Effect, Grant, NewGrant, Permission, Selector, split_path,
};
use cratefield_module_orgs::service_accounts as accounts;
use livingbrain_pages::access::{
    AccessPlan, KIND_FACTS, KIND_PAGES, KIND_SOURCES, SHARED, agent_principal, member_principal,
    page_exists, personal, slug_path, space_scope,
};
use livingbrain_tokens::Member;
use serde::Deserialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::audit::{Event, record};
use crate::common::{State, database, forbidden, invalid, not_found, port, principal_of};

const MIGRATION_SERVICE_ACCOUNTS: SqlMigration = SqlMigration::new(
    "0001",
    "service-accounts",
    cratefield_module_orgs::MIGRATION_SERVICE_ACCOUNTS.sql,
);
const MIGRATION_GRANTS: SqlMigration = SqlMigration::new(
    "0002",
    "grants",
    cratefield_module_orgs::MIGRATION_GRANTS.sql,
);
const MIGRATION_AUDIT: SqlMigration =
    SqlMigration::new("0003", "audit", cratefield_module_orgs::MIGRATION_AUDIT.sql);

/// The roles a grant may name as a group.
const ROLES: [&str; 4] = ["owner", "admin", "member", "viewer"];

/// The `access` module, mounted at `/v1/access`.
#[derive(Debug, Default)]
pub struct Access;

impl Access {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Access {
    fn name(&self) -> &'static str {
        "access"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock, Port::IdGen]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[
            accounts::ACCOUNTS_TABLE,
            accounts::KEYS_TABLE,
            grants::TABLE,
            cratefield_module_orgs::audit::TABLE,
        ]
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        &cratefield_module_orgs::ACCESS_PERSONAL_DATA
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    /// The harness's three migrations, portable SQL, one file for both
    /// dialects.
    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 3] = [
            MIGRATION_SERVICE_ACCOUNTS,
            MIGRATION_GRANTS,
            MIGRATION_AUDIT,
        ];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS,
        }
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        axum::Router::new()
            .route("/grants", get(list).post(create))
            .route("/grants/{id}", axum::routing::delete(remove))
            .route("/pages/{slug}", get(who_can_see))
            .with_state(Arc::new(State { ctx }))
    }
}

fn grant_json(grant: &Grant) -> Value {
    json!({
        "id": grant.id,
        "principal": grant.principal,
        "kind": grant.selector.kind(),
        "space": grant.selector.space(),
        "path": grant.selector.path_string(),
        "permission": grant.permission.as_str(),
        "effect": grant.effect.as_str(),
        "expires_at": grant.expires_at,
        "created_by": grant.created_by,
        "created_at": grant.created_at,
    })
}

/// The caller's plan: a session, so their whole reach.
async fn plan_of(state: &State, caller: &Member) -> Result<AccessPlan, Problem> {
    let db = state.db()?;
    AccessPlan::load(&*db, &principal_of(caller), state.now()?)
        .await
        .map_err(database)
}

/// `GET /grants`.
async fn list(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let db = state.db()?;
    let all = grants::grants_in_org(&*db, &caller.workspace_id)
        .await
        .map_err(database)?;
    let visible: Vec<&Grant> = if caller.is_admin {
        all.iter().collect()
    } else {
        let mine: BTreeSet<String> =
            accounts::list(&*db, &caller.workspace_id, Some(&caller.user_id))
                .await
                .map_err(database)?
                .iter()
                .map(|agent| agent_principal(&agent.id))
                .chain([
                    member_principal(&caller.user_id),
                    format!("role:{}", caller.role.as_str()),
                ])
                .collect();
        let own_space = personal(&caller.user_id);
        all.iter()
            .filter(|grant| mine.contains(&grant.principal) || grant.selector.space() == own_space)
            .collect()
    };
    Ok(
        Json(json!({ "grants": visible.into_iter().map(grant_json).collect::<Vec<_>>() }))
            .into_response(),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantBody {
    principal: String,
    space: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    kind: Option<String>,
    permission: String,
    #[serde(default)]
    effect: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
}

/// The principal a grant names, checked to exist in this workspace: a live
/// member, an agent account, or a role.
async fn checked_principal(
    state: &State,
    workspace_id: &str,
    raw: &str,
) -> Result<String, Problem> {
    let db = state.db()?;
    let raw = raw.trim();
    if let Some(id) = raw.strip_prefix("member:") {
        livingbrain_workspaces::caller_for(&*db, workspace_id, id)
            .await
            .map_err(database)?
            .ok_or_else(|| invalid("`principal` names nobody in this workspace"))?;
        return Ok(member_principal(id));
    }
    if let Some(id) = raw.strip_prefix("agent:") {
        accounts::get(&*db, workspace_id, id)
            .await
            .map_err(database)?
            .ok_or_else(|| invalid("`principal` names no agent account in this workspace"))?;
        return Ok(agent_principal(id));
    }
    if let Some(role) = raw.strip_prefix("role:")
        && ROLES.contains(&role)
    {
        return Ok(format!("role:{role}"));
    }
    Err(invalid(
        "`principal` is `member:<id>`, `agent:<id>` or `role:<owner|admin|member|viewer>`",
    ))
}

/// The space a request names: `shared`, `personal:<id>` (or `personal:me`).
fn checked_space(caller: &Member, raw: &str) -> Result<String, Problem> {
    let raw = raw.trim();
    let space = if raw == "personal:me" {
        personal(&caller.user_id)
    } else {
        raw.to_owned()
    };
    if space == SHARED
        || space
            .strip_prefix("personal:")
            .is_some_and(|id| !id.is_empty())
    {
        Ok(space)
    } else {
        Err(invalid("`space` is `shared` or `personal:<member id>`"))
    }
}

/// A folder path as a request spells it: `coding-agents/claude`, every
/// segment in the slug alphabet so a page grant names folders a slug can
/// spell, and a fact or source grant names subjects and projects.
fn checked_path(raw: &str) -> Result<Vec<String>, Problem> {
    let path = split_path(raw.trim());
    let fine = path.iter().all(|segment| {
        !segment.is_empty()
            && segment.len() <= 128
            && segment
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
            && !segment.contains("--")
            && segment != "."
            && segment != ".."
    });
    if fine && path.len() <= 16 {
        Ok(path)
    } else {
        Err(invalid(
            "`path` is up to 16 `/`-separated segments of letters, digits, `-`, `.`, `_` \
             (a page folder is lowercase, as its slug is)",
        ))
    }
}

/// `POST /grants`.
async fn create(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Json(body): Json<GrantBody>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let principal = checked_principal(&state, &caller.workspace_id, &body.principal).await?;
    let space = checked_space(&caller, &body.space)?;
    let path = checked_path(&body.path)?;
    let kind = match body.kind.as_deref().unwrap_or(ANY_KIND) {
        k @ (KIND_PAGES | KIND_SOURCES | KIND_FACTS) => k.to_owned(),
        "*" => ANY_KIND.to_owned(),
        _ => return Err(invalid("`kind` is `pages`, `sources`, `facts` or `*`")),
    };
    let permission = Permission::parse(body.permission.trim())
        .ok_or_else(|| invalid("`permission` is `read`, `write` or `admin`"))?;
    let effect = match body.effect.as_deref().unwrap_or("allow") {
        "allow" => Effect::Allow,
        "deny" => Effect::Deny,
        _ => return Err(invalid("`effect` is `allow` or `deny`")),
    };
    let now = state.now()?;
    let expires_at = match body.expires_at.as_deref().filter(|at| !at.is_empty()) {
        None => None,
        Some(raw) => {
            let at = OffsetDateTime::parse(raw, &Rfc3339)
                .map_err(|_| invalid("`expires_at` is an RFC 3339 time"))?;
            if at <= now {
                return Err(invalid("`expires_at` is in the past"));
            }
            Some(at)
        }
    };
    let selector = Selector::new(kind, space, path).map_err(|error| invalid(error.to_string()))?;
    // A grant that names the asker's own space as a member of it is
    // pointless and a deny on it is ignored; refusing both keeps the list
    // honest.
    if principal == member_principal(&caller.user_id)
        && selector.space() == personal(&caller.user_id)
    {
        return Err(invalid("that is your own space; you already hold it"));
    }
    let plan = plan_of(&state, &caller).await?;
    if !plan.may_grant(&selector) {
        return Err(forbidden());
    }
    let db = state.db()?;
    let id = state.id("grt_")?;
    let stored = grants::put_grant(
        &*db,
        &caller.workspace_id,
        &id,
        &NewGrant {
            principal: principal.clone(),
            selector: selector.clone(),
            permission,
            effect,
            expires_at,
            created_by: caller.user_id.clone(),
        },
        now,
    )
    .await
    .map_err(database)?;
    let grant = grants::grant(&*db, &caller.workspace_id, &stored)
        .await
        .map_err(database)?
        .ok_or_else(Problem::internal)?;
    audit(
        &state,
        &caller,
        "grant.create",
        &format!(
            "{}:{}/{}",
            grant.selector.kind(),
            grant.selector.space(),
            grant.selector.path_string()
        ),
        &format!(
            "id={} principal={principal} permission={} effect={}",
            grant.id,
            grant.permission.as_str(),
            grant.effect.as_str()
        ),
    )
    .await;
    Ok((StatusCode::CREATED, Json(grant_json(&grant))).into_response())
}

/// `DELETE /grants/{id}`.
async fn remove(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    let db = state.db()?;
    let grant = grants::grant(&*db, &caller.workspace_id, &id)
        .await
        .map_err(database)?
        .ok_or_else(not_found)?;
    let plan = plan_of(&state, &caller).await?;
    // Anybody may give up access made to them — never a deny, which would be
    // an escape; otherwise it is the same authority creating it took.
    let theirs =
        grant.effect == Effect::Allow && grant.principal == member_principal(&caller.user_id);
    if !theirs && !plan.may_grant(&grant.selector) {
        return Err(not_found());
    }
    grants::delete_grant(&*db, &caller.workspace_id, &id)
        .await
        .map_err(database)?;
    audit(
        &state,
        &caller,
        "grant.delete",
        &format!("grant:{id}"),
        &format!("principal={}", grant.principal),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `GET /pages/{slug}` — who can see this page, for someone who can.
async fn who_can_see(
    AxumState(state): AxumState<Arc<State>>,
    method: Method,
    headers: HeaderMap,
    Path(slug): Path<String>,
) -> Result<Response, Problem> {
    let caller = state.caller(&method, &headers).await?;
    if !livingbrain_pages::is_slug(&slug) {
        return Err(not_found());
    }
    let plan = plan_of(&state, &caller).await?;
    let db = state.db()?;
    let mut found = None;
    for filter in plan.filters(KIND_PAGES, Permission::Read) {
        if filter.matches(&slug)
            && page_exists(&*db, &filter.scope, &slug)
                .await
                .map_err(database)?
        {
            found = Some(filter);
            break;
        }
    }
    let filter = found.ok_or_else(not_found)?;
    let space = filter.space.clone();
    let path = slug_path(&slug);
    let mut readers: Vec<Value> = Vec::new();
    if space == SHARED {
        for (role, permission) in [
            ("owner", "admin"),
            ("admin", "admin"),
            ("member", "write"),
            ("viewer", "read"),
        ] {
            readers.push(json!({ "principal": format!("role:{role}"), "permission": permission, "via": "role" }));
        }
    } else if let Some(owner) = space.strip_prefix("personal:") {
        readers.push(
            json!({ "principal": member_principal(owner), "permission": "admin", "via": "owner" }),
        );
    }
    let now = state.now()?;
    for grant in grants::grants_on_space(&*db, &caller.workspace_id, &space)
        .await
        .map_err(database)?
    {
        let covers = (grant.selector.kind() == ANY_KIND || grant.selector.kind() == KIND_PAGES)
            && path.starts_with(grant.selector.path());
        if covers && grant.is_live(now) {
            readers.push(json!({
                "principal": grant.principal,
                "permission": grant.permission.as_str(),
                "effect": grant.effect.as_str(),
                "via": "grant",
                "grant_id": grant.id,
                "folder": grant.selector.path_string(),
                "expires_at": grant.expires_at,
            }));
        }
    }
    let scope_of_space = space_scope(&caller.workspace_id, &space);
    Ok(Json(json!({
        "slug": slug,
        "space": space,
        "you_can_share": scope_of_space.is_some()
            && plan.may_grant(&Selector::new(KIND_PAGES, space.clone(), path).map_err(|_| Problem::internal())?),
        "readers": readers,
    }))
    .into_response())
}

async fn audit(state: &State, caller: &Member, action: &str, resource: &str, detail: &str) {
    let (Ok(db), Ok(now), Ok(id_gen)) = (
        state.db(),
        state.now(),
        port::<dyn IdGen>(state.ctx.ports.id_gen.clone()),
    ) else {
        return;
    };
    record(
        &*db,
        &*id_gen,
        now,
        Event {
            workspace_id: &caller.workspace_id,
            actor_kind: "member",
            actor_id: &caller.user_id,
            on_behalf_of: None,
            credential: "session",
            action,
            resource,
            detail,
        },
    )
    .await;
}
