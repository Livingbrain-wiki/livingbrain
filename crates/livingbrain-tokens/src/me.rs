//! `GET /v1/me` (ADR 0004): who a credential speaks for, readable with a
//! bearer token as well as the session cookie, so the Mac app, the CLI and
//! Colonizer can show "connected as" without a browser.
//!
//! It answers ids, names and the credential's public prefix — never a
//! secret — and the same 401 every other route answers for a credential
//! that names nobody.

use std::sync::Arc;

use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::HeaderMap;
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem,
};
use serde_json::json;

use crate::{AuthError, authenticate};

/// The `me` module, mounted at `/v1/me`. No tables of its own.
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

    /// The same three ports [`authenticate`] reads.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Signer, Port::Clock]
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
            .route("/", get(me))
            .with_state(Arc::new(ctx))
    }
}

/// `GET /` — the workspace, the principal and the credential.
async fn me(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let member = authenticate(&ctx.ports, &headers).await?;
    let db = ctx.ports.db.clone().ok_or(AuthError)?;
    let caller = livingbrain_workspaces::caller_for(&*db, &member.workspace_id, &member.user_id)
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
    Ok(Json(json!({
        "workspace": { "id": caller.workspace_id, "name": caller.workspace_name },
        "principal": {
            "kind": "member",
            "id": caller.user_id,
            "name": caller.name,
            "role": member.role.as_str(),
            "is_admin": member.is_admin,
        },
        "credential": credential,
    }))
    .into_response())
}
