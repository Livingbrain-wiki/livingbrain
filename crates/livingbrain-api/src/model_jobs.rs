//! The `model-jobs` module: where a member's own runner picks up the model
//! calls of their jobs and hands the answers back (`docs/local-runner.md`).
//!
//! A consumer subscription (Claude Pro/Max, ChatGPT Plus/Pro) may only be
//! used inside its vendor's own client, so its credential never comes here.
//! A member who runs their jobs on their own device (`PUT
//! /v1/models/mine/{role}`) has their `livingbrain jobs run` — or the Mac
//! app, which runs it — poll these routes and make each call with their own
//! `claude` or `codex` CLI:
//!
//! - `GET /v1/model-jobs/next` — claim the oldest of **the caller's own**
//!   waiting jobs under a lease: `200 {"job": …}` with the prompt, or `204`.
//! - `POST /v1/model-jobs/{id}/result` — the answer: redacted on arrival,
//!   validated for its kind (a compile's answer must be the closed JSON edit
//!   list), stored for the caller that asked, and — for a compile — the
//!   compile carries on at once.
//! - `POST /v1/model-jobs/{id}/fail` — the runner could not answer.
//! - `GET /v1/model-jobs/{id}` and `GET /v1/model-jobs` — the caller's own
//!   jobs' statuses, never their prompts or answers.
//!
//! **Isolation.** Every statement names the caller's workspace and user id
//! as the job's owner, so another member — or another workspace — can
//! neither claim nor read a job: an id that is not theirs is the same 404 as
//! an id that does not exist. A runner needs an unrestricted credential: a
//! job's prompt can carry anything its owner can read, so a token narrowed
//! to fewer scopes may not take one.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::extract::{Path, Query, State};
use cratefield_core::axum::http::{HeaderMap, Method, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::{
    Config, ConfigError, Migrations, Module, ModuleContext, Port, Problem, ProblemDef,
    ScheduledBudget,
};
use livingbrain_compiler::{ClaimFilter, Queue};
use livingbrain_models::jobs::{self, JobError, JobInfo};
use livingbrain_redact::{Policy, redact};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Wiki;
use crate::coding_agents::compiler;
use crate::port;
use crate::service::Service;

/// No job of the caller's by that id (or not in a state that takes this).
const NOT_FOUND: ProblemDef = ProblemDef {
    slug: "model-jobs/not-found",
    status: StatusCode::NOT_FOUND,
    title: "No such job",
    description: "There is no job of yours with that id waiting for this.",
};

/// The runner's answer is not what the job asked for.
const BAD_OUTPUT: ProblemDef = ProblemDef {
    slug: "model-jobs/bad-output",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "The answer does not fit the job",
    description: "The answer was not in the shape this job asked for, so the job is failed.",
};

/// A narrowed token may not run jobs.
const NARROWED: ProblemDef = ProblemDef {
    slug: "model-jobs/narrowed-token",
    status: StatusCode::FORBIDDEN,
    title: "A full-access credential is required",
    description: "A job's prompt can carry anything you can read, so a token narrowed to fewer \
                  scopes cannot run jobs. Sign in again with `livingbrain login`.",
};

/// The most jobs the listing answers.
const MAX_LIST: usize = 50;

/// The module, constructed with the venture's [`Wiki`].
pub struct ModelJobs {
    wiki: Wiki,
}

impl ModelJobs {
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for ModelJobs {
    fn name(&self) -> &'static str {
        "model-jobs"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The jobs table is the models module's; the compile that carries on
    /// when an answer arrives reads sources and writes pages over this
    /// module's blob view, which the composition roots on the pages key
    /// space ([`crate::PagesRebind`]).
    fn requires(&self) -> &'static [Port] {
        &[
            Port::Db,
            Port::Blob,
            Port::Clock,
            Port::IdGen,
            Port::HttpClient,
        ]
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
            .route("/", get(list))
            .route("/next", get(next))
            .route("/{id}", get(one))
            .route("/{id}/result", post(result))
            .route("/{id}/fail", post(fail))
            .with_state(state)
    }
}

/// The caller, held to an unrestricted credential.
async fn runner(
    service: &Service,
    method: &Method,
    headers: &HeaderMap,
) -> Result<livingbrain_mcp::Asker, Problem> {
    let asker = service.asker(method, headers).await?;
    // A runner is a member's own device (ADR 0004: never an agent account).
    Service::member_only(&asker)?;
    if asker.token_scopes.is_some() {
        return Err(Problem::new(&NARROWED));
    }
    Ok(asker)
}

fn job_error(error: JobError) -> Problem {
    match error {
        JobError::NotFound => Problem::new(&NOT_FOUND),
        JobError::Invalid(message) => Problem::validation_failed(message),
        JobError::NoSecret | JobError::Store => Problem::internal(),
    }
}

/// A job as the routes show it: never its prompt or answer.
fn view(job: &JobInfo) -> Value {
    json!({
        "id": job.id,
        "kind": job.kind,
        "role": job.role,
        "agent": job.agent,
        "model": job.model,
        "status": job.status,
        "attempts": job.attempts,
        "error": job.error,
        "tokens": {"prompt": job.prompt_tokens, "completion": job.completion_tokens},
        "created_at": job.created_at,
        "updated_at": job.updated_at,
        "lease_until": job.lease_until,
    })
}

/// `GET /next` — claim the oldest of the caller's own waiting jobs.
async fn next(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let asker = runner(&service, &Method::GET, &headers).await?;
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let claimed = jobs::claim_next(
        &*db,
        &*ctx.config,
        clock.now(),
        &asker.workspace_id,
        &asker.user_id,
    )
    .await
    .map_err(job_error)?;
    let Some(job) = claimed else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    Ok(Json(json!({
        "job": {
            "id": job.id,
            "kind": job.kind,
            "agent": job.agent,
            "model": job.model,
            "settings": job.settings,
            "system": job.system,
            "user": job.user,
            "max_tokens": job.max_tokens,
            "json": job.json,
            "lease_until": job.lease_until,
        }
    }))
    .into_response())
}

/// A runner's answer.
#[derive(Debug, Deserialize)]
struct ResultBody {
    text: String,
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

/// `POST /{id}/result` — store the answer, then let a compile carry on.
async fn result(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<ResultBody>,
) -> Result<Response, Problem> {
    let asker = runner(&service, &Method::POST, &headers).await?;
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let id_gen = port(ctx.ports.id_gen.clone())?;
    let job = jobs::get(&*db, &asker.workspace_id, &asker.user_id, &id)
        .await
        .map_err(|_| Problem::internal())?
        .filter(|job| job.status == "claimed")
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    // Redacted on arrival, whatever the runner's machine already did: the
    // answer is model output and lands in pages and replies.
    let text = match redact(&body.text, Policy::Redact) {
        Ok((clean, _)) => clean,
        Err(_) => String::new(),
    };
    if !valid(&job.kind, &text) {
        let _ = jobs::fail(
            &*db,
            clock.now(),
            &asker.workspace_id,
            &asker.user_id,
            &id,
            "bad-output",
        )
        .await;
        // The compile notices the failed call on its next run.
        let _ = continue_compile(&service, &asker.workspace_id, &job).await;
        return Err(Problem::new(&BAD_OUTPUT));
    }
    let done = jobs::complete(
        &*db,
        &*ctx.config,
        &*id_gen,
        clock.now(),
        &asker.workspace_id,
        &asker.user_id,
        &id,
        &text,
        body.prompt_tokens,
        body.completion_tokens,
    )
    .await
    .map_err(job_error)?;
    let continued = continue_compile(&service, &asker.workspace_id, &job).await;
    Ok(Json(json!({
        "status": done.status,
        "job": view(&done),
        "continued": continued,
    }))
    .into_response())
}

/// Whether an answer fits its job. A compile's must parse as the closed
/// edit list (`livingbrain_compiler::edits`), the same check the compile
/// itself applies; an ask's must say something — its citations are checked
/// by the ask when it reads the answer, against the pages it was shown.
fn valid(kind: &str, text: &str) -> bool {
    match kind {
        "compile" => livingbrain_compiler::edits::parse(text).is_ok(),
        _ => !text.trim().is_empty(),
    }
}

/// A runner's failure.
#[derive(Debug, Deserialize)]
struct FailBody {
    reason: String,
}

/// `POST /{id}/fail` — the runner could not answer.
async fn fail(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<FailBody>,
) -> Result<Response, Problem> {
    let asker = runner(&service, &Method::POST, &headers).await?;
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let job = jobs::fail(
        &*db,
        clock.now(),
        &asker.workspace_id,
        &asker.user_id,
        &id,
        body.reason.trim(),
    )
    .await
    .map_err(job_error)?;
    let continued = continue_compile(&service, &asker.workspace_id, &job).await;
    Ok(
        Json(json!({"status": job.status, "job": view(&job), "continued": continued}))
            .into_response(),
    )
}

/// `GET /{id}` — one of the caller's jobs.
async fn one(
    State(service): State<Arc<Service>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let asker = service.asker(&Method::GET, &headers).await?;
    Service::member_only(&asker)?;
    let db = port(service.ctx().ports.db.clone())?;
    let job = jobs::get(&*db, &asker.workspace_id, &asker.user_id, &id)
        .await
        .map_err(|_| Problem::internal())?
        .ok_or_else(|| Problem::new(&NOT_FOUND))?;
    Ok(Json(view(&job)).into_response())
}

/// `GET /?limit=` — the caller's recent jobs, and their runners' last poll.
async fn list(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Response, Problem> {
    let asker = service.asker(&Method::GET, &headers).await?;
    Service::member_only(&asker)?;
    let db = port(service.ctx().ports.db.clone())?;
    let limit = query
        .get("limit")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(20)
        .clamp(1, MAX_LIST);
    let listed = jobs::list(&*db, &asker.workspace_id, &asker.user_id, limit)
        .await
        .map_err(|_| Problem::internal())?;
    let runners = jobs::runners(&*db, &asker.workspace_id, &asker.user_id)
        .await
        .map_err(|_| Problem::internal())?;
    let last_seen = runners
        .iter()
        .filter_map(|runner| runner.last_seen_at.clone())
        .max();
    Ok(Json(json!({
        "jobs": listed.iter().map(view).collect::<Vec<_>>(),
        "runner": {"last_seen_at": last_seen},
    }))
    .into_response())
}

/// For a compile call, runs that compile again now — its answer (or its
/// failure) is in, so it applies the chunk and moves on to the next, which
/// is either another call handed to the runner or the end of the job.
/// Answers what became of it; `null` when there was nothing to continue.
async fn continue_compile(service: &Service, workspace: &str, job: &JobInfo) -> Value {
    if job.kind != "compile" {
        return Value::Null;
    }
    // `<scope>:<body_sha256>:<chunk>`; the scope is the part before the
    // last two separators.
    let mut parts = job.ref_key.rsplitn(3, ':');
    let (Some(_chunk), Some(body), Some(scope)) = (parts.next(), parts.next(), parts.next()) else {
        return Value::Null;
    };
    let ctx = service.ctx();
    let Some(db) = ctx.ports.db.clone() else {
        return Value::Null;
    };
    let Ok(Some(compile)) = Queue::new(db).get(scope, body).await else {
        return Value::Null;
    };
    // The job must be this member's own, in this workspace — the ref key
    // came from the job row, but nothing is taken on trust.
    if compile.workspace != workspace || compile.owner != job.owner {
        return Value::Null;
    }
    let Some(compiler) = compiler(ctx, &service.wiki()) else {
        return Value::Null;
    };
    let filter = ClaimFilter {
        workspace: Some(workspace.to_owned()),
        scopes: Some(vec![scope.to_owned()]),
        ignore_backoff: true,
        source_id: Some(compile.source_id.clone()),
    };
    match compiler
        .drain(&filter, 1, &ScheduledBudget::unbounded())
        .await
    {
        Ok(processed) => json!(
            processed
                .iter()
                .map(|p| json!({"source_id": p.source_id, "status": p.status.as_str()}))
                .collect::<Vec<_>>()
        ),
        Err(_) => Value::Null,
    }
}
