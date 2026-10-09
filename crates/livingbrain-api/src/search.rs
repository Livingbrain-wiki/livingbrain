//! The `search` module: `GET /v1/search`, the route the CLI's
//! `livingbrain search` speaks.
//!
//! The same recall `brain_search` gives an agent — every page in the
//! asker's scopes whose body holds **every** word of the query — as data:
//! a title, a citation URL and a snippet per hit, and the optional
//! `project` filter over the frontmatter a note carries.

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::HeaderMap;
use cratefield_core::axum::routing::get;
use cratefield_core::{Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem};
use serde_json::{Value, json};

use crate::Wiki;
use crate::recipes::{DEFAULT_LIMIT, MAX_LIMIT, find, keep_project, result_json};
use crate::service::Service;

/// The `search` module, constructed with the pages access the venture
/// injected.
pub struct Search {
    wiki: Wiki,
}

impl Search {
    /// A search module over the given pages access. See [`Wiki`] for where
    /// each field comes from.
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for Search {
    fn name(&self) -> &'static str {
        "search"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The store is built per request from these; the blob is this module's
    /// own per-request view, which the composition has rooted on the pages
    /// key space by planting [`RebindBlob`](crate::RebindBlob) under the
    /// scope the harness applies. Undeclared, the view would never arrive.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Blob, Port::Clock, Port::IdGen]
    }

    /// `Signer` is optional for the same reason the pages module's is: the
    /// credential resolver runs on this module's port view, and the views
    /// only carry what a module declared. Without the declaration the view
    /// would arrive with no signer, and `BearerAuth` could never verify a
    /// caller.
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
            .route("/", get(search))
            .with_state(state)
    }
}

/// `GET /?q=…&project=…&limit=…` — the pages in the asker's scopes whose
/// body holds every word of `q`, newest index first, each as a title, a
/// citation URL and a snippet.
///
/// `project`, when sent, keeps only the pages whose frontmatter names it —
/// the key a filed note carries. `limit` defaults to 10 and is **clamped,
/// not refused**: a caller asking for a thousand hits wants hits, so
/// anything above 25 gets 25; a limit that does not parse is a 422.
///
/// # Errors
///
/// 401 without a credential, 422 for a missing or empty `q` or an
/// unparseable `limit`, and the store's mapped problem otherwise.
async fn search(
    State(state): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = state.asker(&headers).await?;
    let q = query
        .get("q")
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .ok_or_else(|| Problem::validation_failed("a non-empty `q` is required"))?;
    let limit = match query.get("limit") {
        None => DEFAULT_LIMIT,
        Some(raw) => clamp_limit(raw.parse::<u32>().map_err(|_| {
            Problem::validation_failed(format!("`limit` must be a positive integer, got {raw:?}"))
        })?),
    };
    let project = query.get("project").map(String::as_str);

    let store = state.store()?;
    let scopes = state.scopes(&asker);
    let found = find(&store, &scopes, q, limit).await?;
    let found = keep_project(found, project);
    let results: Vec<Value> = found.iter().map(result_json).collect();
    Ok(Json(json!({ "results": results })))
}

/// A caller-supplied limit, capped rather than refused: 1..=[`MAX_LIMIT`],
/// the same clamp `brain_search` applies.
fn clamp_limit(asked: u32) -> usize {
    (asked as usize).clamp(1, MAX_LIMIT)
}

#[cfg(test)]
mod tests {
    use super::{MAX_LIMIT, clamp_limit};

    #[test]
    fn the_limit_clamps_to_the_ceiling_rather_than_refusing() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(2), 2);
        assert_eq!(clamp_limit(MAX_LIMIT as u32), MAX_LIMIT);
        assert_eq!(clamp_limit(u32::MAX), MAX_LIMIT);
    }
}
