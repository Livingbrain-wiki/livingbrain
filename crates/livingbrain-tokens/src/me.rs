//! `GET /v1/me` (ADR 0004): who a credential speaks for, readable with a
//! bearer token as well as the session cookie, so the Mac app, the CLI and
//! Colonizer can show "connected as" without a browser.
//!
//! It answers ids, names and the credential's public prefix — never a
//! secret — and the same 401 every other route answers for a credential
//! that names nobody.
//!
//! It is also where a member changes how they appear: `PATCH /v1/me` sets
//! (or, with `null`, clears) the name they chose, and `PUT`/`DELETE
//! /v1/me/avatar` their avatar. Those writes take the session cookie from
//! this origin only (the same-origin rule the token routes keep), or a
//! bearer token that was **not** narrowed: a token cut down to a read scope
//! reads, and does not rewrite its owner's profile.
//!
//! The answer only ever grows. Every field a client read before is still
//! there with its meaning (`principal.name` is the name everybody reads,
//! the chosen one when there is one); `display_name` and the two
//! `avatar_url`s are additions, `null` when unset.

use std::sync::Arc;

use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::{DefaultBodyLimit, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem,
    ProblemDef,
};
use livingbrain_workspaces as workspaces;
use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;

use crate::{AuthError, Member, authenticate};

/// A cookie write a browser reports as coming from another site.
const CROSS_SITE_REQUEST: ProblemDef = ProblemDef {
    slug: "me/cross-site-request",
    status: StatusCode::FORBIDDEN,
    title: "A same-origin request is required",
    description: "Changing your name or avatar with the session cookie is accepted only from \
                  this venture's own pages.",
};

/// A narrowed token asked to write.
const NARROWED_TOKEN: ProblemDef = ProblemDef {
    slug: "me/narrowed-token",
    status: StatusCode::FORBIDDEN,
    title: "This token cannot change your profile",
    description: "A token cut down to a scope subset reads. Use a full token, or the app.",
};

/// The `me` module, mounted at `/v1/me`. No tables of its own: the name and
/// avatar it writes are the member row's, through the workspaces module's
/// own functions.
#[derive(Debug, Default)]
pub struct Me;

impl Me {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Me {
    fn name(&self) -> &'static str {
        "me"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The three ports [`authenticate`] reads, and `IdGen` for a new
    /// avatar's id.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock, Port::IdGen]
    }

    /// The blob store holds the avatar image; without one the avatar routes
    /// answer `503` and everything else here works.
    fn optional(&self) -> &'static [Port] {
        &[Port::Blob]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[]
    }

    fn personal_data(&self) -> &'static [PersonalDataSet] {
        &[]
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn migrations(&self) -> Migrations {
        Migrations::sqlite(&[])
    }

    /// The avatar upload takes an image; see
    /// [`workspaces::AVATAR_BODY_CEILING`].
    fn max_body_bytes(&self, _cfg: &dyn Config) -> usize {
        workspaces::AVATAR_BODY_CEILING
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        axum::Router::new()
            .route("/", get(me).patch(rename))
            .route(
                "/avatar",
                axum::routing::put(put_avatar)
                    .delete(delete_avatar)
                    .layer(DefaultBodyLimit::max(workspaces::AVATAR_BODY_CEILING)),
            )
            .with_state(Arc::new(ctx))
    }
}

/// The body `GET /` answers, for `member`.
async fn me_body(ctx: &ModuleContext, member: &Member) -> Result<Value, Problem> {
    let db = ctx.ports.db.clone().ok_or(AuthError)?;
    let caller = workspaces::caller_for(&*db, &member.workspace_id, &member.user_id)
        .await
        .map_err(|_| Problem::internal())?
        .ok_or(AuthError)?;
    let credential = match &member.credential {
        None => json!({ "kind": "session" }),
        Some(prefix) => json!({
            "kind": "personal_access_token",
            "prefix": prefix,
            "scopes": member.scopes,
        }),
    };
    Ok(json!({
        "workspace": {
            "id": caller.workspace_id,
            "name": caller.workspace_name,
            "avatar_url": caller.workspace_avatar.map(|avatar| avatar.url),
        },
        "principal": {
            "kind": "member",
            "id": caller.user_id,
            "name": caller.name,
            "display_name": caller.display_name,
            "avatar_url": caller.avatar.map(|avatar| avatar.url),
            "role": member.role.as_str(),
            "is_admin": member.is_admin,
        },
        "credential": credential,
    }))
}

/// `GET /` — the workspace, the principal and the credential.
async fn me(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let member = authenticate(&ctx.ports, &headers).await?;
    Ok(Json(me_body(&ctx, &member).await?).into_response())
}

/// The caller of a profile write: a session from this origin, or a bearer
/// token that was not narrowed.
async fn writer(ctx: &ModuleContext, headers: &HeaderMap) -> Result<Member, Problem> {
    let member = authenticate(&ctx.ports, headers).await?;
    match &member.credential {
        None => crate::same_origin(headers)
            .map_err(|refused| Problem::new(&CROSS_SITE_REQUEST).with_detail(refused.detail))?,
        Some(_) if member.scopes.is_some() => return Err(Problem::new(&NARROWED_TOKEN)),
        Some(_) => {}
    }
    Ok(member)
}

fn now(ctx: &ModuleContext) -> Result<String, Problem> {
    let clock = ctx.ports.clock.clone().ok_or_else(Problem::internal)?;
    Ok(clock.now().format(&Rfc3339).unwrap_or_default())
}

/// `PATCH /` — `{"display_name": "Ada"}` sets the name this member is shown
/// by in this workspace; `{"display_name": null}` goes back to the one the
/// sign-in method reports. Answers what `GET /` answers.
async fn rename(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Response, Problem> {
    let member = writer(&ctx, &headers).await?;
    let chosen = match body.get("display_name") {
        None => return Err(workspaces::nothing_to_change()),
        Some(Value::Null) => None,
        Some(Value::String(raw)) => {
            Some(workspaces::clean_name(raw).map_err(workspaces::name_problem)?)
        }
        Some(_) => return Err(workspaces::name_problem(workspaces::NameError::Empty)),
    };
    let db = ctx.ports.db.clone().ok_or_else(Problem::internal)?;
    let found = workspaces::set_display_name(
        &*db,
        &member.workspace_id,
        &member.user_id,
        chosen.as_deref(),
        &now(&ctx)?,
    )
    .await
    .map_err(|_| Problem::internal())?;
    if !found {
        return Err(AuthError.into());
    }
    Ok(Json(me_body(&ctx, &member).await?).into_response())
}

/// `PUT /avatar` — the member's avatar, as the raw image body with its own
/// `Content-Type`. Checked by its bytes, rewritten without metadata, stored
/// under a fresh id; the one it replaces is deleted. Answers what `GET /`
/// answers.
async fn put_avatar(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, Problem> {
    let member = writer(&ctx, &headers).await?;
    let blob = ctx
        .ports
        .blob
        .clone()
        .ok_or_else(workspaces::avatars_unavailable)?;
    let declared = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let image = workspaces::clean_image(&body, declared).map_err(workspaces::image_problem)?;
    let id_gen = ctx.ports.id_gen.clone().ok_or_else(Problem::internal)?;
    let db = ctx.ports.db.clone().ok_or_else(Problem::internal)?;
    let id = workspaces::store_avatar(&*blob, &member.workspace_id, &id_gen.ulid(), &image).await?;
    let previous = workspaces::set_member_avatar(
        &*db,
        &member.workspace_id,
        &member.user_id,
        Some(&id),
        &now(&ctx)?,
    )
    .await
    .map_err(|_| Problem::internal())?;
    let Some(previous) = previous else {
        // The member went between the check and the write: nothing names
        // the new image, so it goes too.
        workspaces::forget_avatar(&*blob, &member.workspace_id, Some(&id), None).await;
        return Err(AuthError.into());
    };
    workspaces::forget_avatar(&*blob, &member.workspace_id, previous.as_deref(), Some(&id)).await;
    Ok(Json(me_body(&ctx, &member).await?).into_response())
}

/// `DELETE /avatar` — back to the initials.
async fn delete_avatar(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let member = writer(&ctx, &headers).await?;
    let db = ctx.ports.db.clone().ok_or_else(Problem::internal)?;
    let previous = workspaces::set_member_avatar(
        &*db,
        &member.workspace_id,
        &member.user_id,
        None,
        &now(&ctx)?,
    )
    .await
    .map_err(|_| Problem::internal())?
    .ok_or(AuthError)?;
    if let Some(blob) = ctx.ports.blob.clone() {
        workspaces::forget_avatar(&*blob, &member.workspace_id, previous.as_deref(), None).await;
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}
