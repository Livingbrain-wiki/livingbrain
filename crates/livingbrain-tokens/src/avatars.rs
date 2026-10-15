//! `GET /v1/avatars/{id}`: an avatar image, to a member of the workspace
//! whose row names it — and to nobody else.
//!
//! The id alone grants nothing. The caller is resolved first (the session
//! cookie or a bearer token, through [`authenticate`]); the id must then be
//! the avatar of the caller's own workspace or of a **live** member of it.
//! Another workspace's avatar, a removed member's, one since replaced, and
//! one that never existed are the same `404`.
//!
//! The bytes are served as the image type they **are** — sniffed again on
//! the way out, never the stored label alone — with `nosniff`, an inline
//! disposition, and a sandboxing CSP so the response can never be a
//! document. A new upload is a new id, so a URL never changes what it shows
//! and may be cached as immutable — privately, since it is member-only.

use std::sync::Arc;

use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderValue, StatusCode, header};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, http::HeaderMap};
use cratefield_core::{
    Config, ConfigError, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem,
    ProblemDef,
};
use livingbrain_workspaces as workspaces;

use crate::authenticate;

/// The one refusal for every id the caller may not read.
const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "avatars/not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such avatar",
    description: "There is no avatar with this id in your workspace.",
};

/// The `avatars` module, mounted at `/v1/avatars`. No tables: the rows that
/// name an avatar are the workspaces module's.
#[derive(Debug, Default)]
pub struct Avatars;

impl Avatars {
    /// The module.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Avatars {
    fn name(&self) -> &'static str {
        "avatars"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The three ports [`authenticate`] reads.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock]
    }

    /// The images themselves; without a store every id is a `404`.
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

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        axum::Router::new()
            .route("/{id}", get(avatar))
            .with_state(Arc::new(ctx))
    }
}

fn not_found() -> Problem {
    Problem::new(&NOT_FOUND)
}

/// `GET /{id}` — the image, to a member of the workspace that names it.
async fn avatar(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    let member = authenticate(&ctx.ports, &headers).await?;
    if !workspaces::is_avatar_id(&id) {
        return Err(not_found());
    }
    let db = ctx.ports.db.clone().ok_or_else(Problem::internal)?;
    if !workspaces::readable_avatar(&*db, &member.workspace_id, &id)
        .await
        .map_err(|_| Problem::internal())?
    {
        return Err(not_found());
    }
    let blob = ctx.ports.blob.clone().ok_or_else(not_found)?;
    let object = blob
        .get(&workspaces::avatar_key(&member.workspace_id, &id))
        .await
        .map_err(|error| workspaces::blob_failed(&error))?
        .ok_or_else(not_found)?;
    // The bytes decide the type, as they did on the way in.
    let kind = workspaces::ImageKind::sniff(&object.bytes).ok_or_else(not_found)?;
    let mut response = object.bytes.into_response();
    let set = |response: &mut Response, name: header::HeaderName, value: &str| {
        if let Ok(value) = HeaderValue::from_str(value) {
            response.headers_mut().insert(name, value);
        }
    };
    set(&mut response, header::CONTENT_TYPE, kind.mime());
    set(&mut response, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(
        &mut response,
        header::CONTENT_DISPOSITION,
        &format!("inline; filename=\"avatar.{}\"", kind.extension()),
    );
    // The harness stamps `no-store` over this on every `/v1` answer; the
    // composition relaxes it for this route
    // ([`workspaces::AVATAR_CACHE_CONTROL`]).
    set(
        &mut response,
        header::CACHE_CONTROL,
        workspaces::AVATAR_CACHE_CONTROL,
    );
    set(
        &mut response,
        header::CONTENT_SECURITY_POLICY,
        "default-src 'none'; sandbox",
    );
    set(
        &mut response,
        header::HeaderName::from_static("cross-origin-resource-policy"),
        "same-origin",
    );
    set(&mut response, header::VARY, "Cookie, Authorization");
    Ok(response)
}
