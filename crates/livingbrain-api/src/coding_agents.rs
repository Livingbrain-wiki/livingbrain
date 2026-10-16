//! The `coding-agents` module: the compiler's queue, its two routes, and
//! its cron drain (see `livingbrain-compiler` for what compiling is).
//!
//! - `GET /v1/coding-agents/queue?limit=&source=` — the asker's jobs, newest
//!   first, with a count per status, whether a model is connected, and the
//!   compile budget and what is spent of it. A read first enqueues whatever
//!   the ledger holds that has no job yet, so a session synced a moment ago
//!   shows as `queued` rather than as nothing.
//! - `POST /v1/coding-agents/process` (`{"max": 1..3}`) — compiles up to
//!   `max` of the asker's own jobs now, in the request — side by side, so
//!   at most three at once — and answers what became of each. A parked job (`skipped-no-model`, `over-budget`) is
//!   re-checked at once, since the person pressing the button may just have
//!   connected a model. Cookie writes must be same-origin, like every cookie
//!   write on this surface.
//! - `POST /v1/coding-agents/retry` — puts the asker's own `failed` jobs
//!   back to `queued` with their attempts reset (the queue card's "Retry
//!   failed"), and answers how many. A failed job's `error` names what the
//!   provider said (`model-http 429: {…}`), so the person can see the cause
//!   is fixed before pressing it.
//! - `GET /v1/coding-agents/tree` — the asker's synced sessions grouped
//!   by agent and project, with how many compiled ([`tree`]): the home
//!   view's `coding-agents/` folders before any page is written.
//!
//! - `POST /v1/coding-agents/compile-fully` (`{"source_id"}`) — "Compile
//!   fully": puts one of the asker's own `skipped-trivial` or `summary-only`
//!   jobs back to `queued`, to be compiled with no triage and no compaction,
//!   and compiles it at once in the request. Triage never loses a session:
//!   its source is untouched, and this is the way back from a wrong call.
//!
//! All five answer only for the asker's own scopes — the same two clauses the
//! source listing applies — so a member sees and spends nothing of anybody
//! else's. The cron tick (`scheduled`) drains every workspace in bounded
//! batches — each workspace's own sessions-per-run setting, else
//! `CODING_AGENTS_BATCH`, never past [`livingbrain_compiler::BATCH_CEILING`]
//! — up to each workspace's `concurrency` of them at once (never more than
//! [`livingbrain_compiler::MAX_IN_FLIGHT`] in all), under the invocation's
//! cooperative budget, then runs the model-free lint over the scopes it
//! compiled into.
//!
//! Every budget and limit here is the workspace's **effective** one
//! (`crate::budgets`): its own setting (Settings → Usage & limits), else the
//! env var, else the default; a paused workspace compiles nothing.
//!
//! The model is the workspace's own, through `livingbrain_models::invoke`,
//! on the **Main** job slot: there is no managed fallback, so a workspace
//! with nothing connected parks its jobs as `skipped-no-model` and the
//! status route says so. A member who runs their jobs on their own device
//! (`livingbrain_models::jobs`) has each call of their compiles handed to
//! their runner instead; the job waits as `waiting-runner` and carries on
//! when the answer arrives (`POST /v1/model-jobs/{id}/result`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration as StdDuration;

use cratefield_core::axum::Json;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, Method};
use cratefield_core::axum::routing::{get, post};
use cratefield_core::{
    AnyError, BoxFuture, Clock, Config, ConfigError, DataKind, Database, Disposition, HttpClient,
    IdGen, Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem, ScheduledBudget,
    SqlMigration,
};
use cratefield_module_orgs::grants::Permission;
use livingbrain_compiler::{
    Call, ClaimFilter, Classifiers, Compiler, Dreams, Emitted, Fact, FactSink, Gate, HeldFact, Job,
    JobStatus, Model, ModelFailure, Queue, Reply, Settings, Window,
};
use livingbrain_memory::timeline::{self, FactStatus, TimedFact};
use livingbrain_models::invoke::{self, ChatRequest, InvokeError, Structured};
use livingbrain_models::jobs::{self, Dispatch, JobCall};
use livingbrain_pages::access::{AccessPlan, KIND_PAGES, KIND_SOURCES, ScopeFilter};
use livingbrain_pages::{
    DreamCandidate, DreamDecisionRow, DreamFinish, FactError, FactInput, FactOutcome, FactStore,
};
use serde_json::{Value, json};

mod tree;

use crate::Wiki;
use crate::budgets::StoredTuning;
use crate::port;
use crate::service::Service;

/// The job slot a compile runs on. Main, because the compile is the
/// workspace's main writing job; a role of its own is a later change.
pub const MODEL_ROLE: &str = "main";

/// The most jobs one `Process now` compiles: each is one to a few model
/// calls inside the request, and they run side by side, so this is also how
/// many run at once (fewer when the workspace's pool is smaller).
const MAX_PROCESS_NOW: usize = 3;
/// How long one compile call may take. A forced-thinking model (GLM-5.3)
/// answered a full-size compile in 55–100 s when measured (2026-10-15), and
/// streams at roughly 35 tokens a second thinking included (a 21.6k-token
/// stream took 622 s), so a 12k-token ceiling can take ~6 minutes. The call
/// streams (`livingbrain_models::stream`), so a stalled one ends after 90 s
/// without bytes, not here. Eight minutes, plus the cron tick's 5-minute
/// budget checked before each job, stays inside a cron invocation's 15. The
/// bound is the `HttpClient` port's own `StreamPolicy` (whole-body ceiling),
/// on every route and the cron tick alike.
const CALL_TIMEOUT: StdDuration = StdDuration::from_secs(480);
// A streamed call's deadline is honoured only up to this ceiling; one past
// it would be cut silently. (An unstreamed call is held to the port's
// buffered ceiling, 30 s.)
const _: () =
    assert!(CALL_TIMEOUT.as_secs() <= livingbrain_models::stream::MAX_STREAM_DEADLINE.as_secs());
// The compiler starts a call in a cron tick only when one of this length
// still ends inside the invocation's wall (`livingbrain_compiler::pool`).
const _: () =
    assert!(CALL_TIMEOUT.as_secs() as i64 <= livingbrain_compiler::MAX_CALL.whole_seconds());
/// The most jobs the status route lists.
const MAX_LIST: usize = 100;

/// The module, constructed with the venture's [`Wiki`].
pub struct CodingAgents {
    wiki: Wiki,
}

impl CodingAgents {
    #[must_use]
    pub fn new(wiki: Wiki) -> Self {
        Self { wiki }
    }
}

impl Module for CodingAgents {
    fn name(&self) -> &'static str {
        "coding-agents"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The page and source stores are built from these, over this module's
    /// own blob view — which the composition roots on the pages key space
    /// ([`crate::PagesRebind`]), for requests and for the cron tick alike.
    /// `HttpClient` reaches the workspace's model.
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

    fn tables(&self) -> &'static [&'static str] {
        &livingbrain_compiler::TABLES
    }

    /// A job row names whose session it is (`owner`, a user id) and the
    /// slugs the compile wrote, and nothing of the session's or the page's
    /// content; the spend row is a workspace's pooled count.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "coding_agent_jobs",
                subject: "owner",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "For each synced coding-agent session: its source id, the session \
                              key it was filed under, its project, the compile's status, \
                              attempts, token counts and the wiki slugs it wrote. No session or \
                              page text. Erasing you deletes the rows.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet::none(
                "coding_agent_spend",
                "the row is one workspace's pooled compile tokens for one UTC day — nothing \
                 in it identifies or describes a person",
            ),
            PersonalDataSet::none(
                "coding_agent_reservations",
                "the row is a hold on one workspace's pooled compile tokens for one model call \
                 in flight — a workspace id, a count and a time, nothing about a person",
            ),
            PersonalDataSet::none(
                "coding_agent_pool",
                "the row is one workspace's compile pool — how many compiles run at once, a \
                 rate-limit pause and an average call time; nothing about a person",
            ),
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const SQLITE: [SqlMigration; 5] = [
            livingbrain_compiler::MIGRATION_INIT,
            livingbrain_compiler::MIGRATION_WAITING_RUNNER,
            livingbrain_compiler::MIGRATION_TRIAGE,
            livingbrain_compiler::MIGRATION_PARALLEL,
            livingbrain_compiler::MIGRATION_REQUEUE_BAD_OUTPUT,
        ];
        const _: () = cratefield_core::assert_migration_set(&SQLITE);
        const POSTGRES: [SqlMigration; 5] = [
            livingbrain_compiler::MIGRATION_INIT_POSTGRES,
            livingbrain_compiler::MIGRATION_WAITING_RUNNER_POSTGRES,
            livingbrain_compiler::MIGRATION_TRIAGE_POSTGRES,
            livingbrain_compiler::MIGRATION_PARALLEL_POSTGRES,
            livingbrain_compiler::MIGRATION_REQUEUE_BAD_OUTPUT_POSTGRES,
        ];
        const _: () = cratefield_core::assert_migration_set(&POSTGRES);
        Migrations {
            sqlite: &SQLITE,
            postgres: &POSTGRES,
        }
    }

    /// Every `CODING_AGENTS_*` number, when set, must be a whole number.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        let mut errors = ConfigError::new();
        for key in [
            "CODING_AGENTS_BATCH",
            "CODING_AGENTS_CONCURRENCY",
            "CODING_AGENTS_CHUNK_CHARS",
            "CODING_AGENTS_MAX_CHUNKS",
            "CODING_AGENTS_CONTEXT_CHARS",
            "CODING_AGENTS_MAX_OUTPUT_TOKENS",
            "CODING_AGENTS_JOB_TOKENS",
            "CODING_AGENTS_DAILY_TOKENS",
            "CODING_AGENTS_MONTHLY_TOKENS",
        ] {
            if let Some(raw) = cfg.get(key)
                && raw.trim().parse::<u64>().is_err()
            {
                errors.push(format!("{key} must be a whole number, got {raw:?}"));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        let state = Arc::new(Service::from_wiki(ctx, &self.wiki));
        cratefield_core::axum::Router::new()
            .route("/queue", get(queue))
            .route("/process", post(process))
            .route("/retry", post(retry))
            .route("/tree", get(tree::tree))
            .route("/compile-fully", post(compile_fully))
            .with_state(state)
    }

    /// The cron drain: every workspace, a bounded batch, oldest first,
    /// then the lint over the scopes this tick compiled into.
    fn scheduled<'a>(
        &'a self,
        ctx: &'a ModuleContext,
        _cron: &'a str,
    ) -> BoxFuture<'a, Result<(), AnyError>> {
        Box::pin(async move {
            let Some(compiler) = compiler(ctx, &self.wiki) else {
                return Ok(());
            };
            // Each workspace's own batch caps its share inside the drain;
            // the tick as a whole is bounded by the hard ceiling and by the
            // invocation's cooperative budget.
            let processed = compiler
                .drain(
                    &ClaimFilter::default(),
                    livingbrain_compiler::BATCH_CEILING,
                    &ctx.scheduled,
                )
                .await
                .map_err(|error| Box::new(error) as AnyError)?;
            // Dreaming: tidy the subjects that are due, from what budget the
            // drain left. A failure is logged, never a failed tick.
            match compiler.dream(&ctx.scheduled, DREAM_PASSES_PER_TICK).await {
                Ok(sweep) if sweep.passes > 0 || sweep.enqueued > 0 || sweep.retracted > 0 => {
                    eprintln!(
                        "dreaming: {} queued, {} passes, {} facts updated, {} rule retractions",
                        sweep.enqueued, sweep.passes, sweep.applied, sweep.retracted
                    )
                }
                Ok(_) => {}
                Err(reason) => eprintln!("dreaming: sweep failed: {reason}"),
            }
            // The lint runs over the scopes this tick compiled into: cheap,
            // model-free, and only where something changed.
            let mut scopes = BTreeSet::new();
            if let Some(ledger) = sources(ctx, &self.wiki) {
                for done in processed.iter().filter(|p| p.status == JobStatus::Done) {
                    if let Ok(Some(source)) = ledger.by_id(&done.source_id).await {
                        scopes.insert(source.scope);
                    }
                }
            }
            for scope in scopes {
                let _ = compiler.lint(&scope).await;
            }
            Ok(())
        })
    }
}

/// The source ledger over a context's ports, or `None` when one is unwired.
fn sources(ctx: &ModuleContext, wiki: &Wiki) -> Option<livingbrain_pages::SourceStore> {
    Some(livingbrain_pages::SourceStore::new(
        ctx.ports.db.clone()?,
        ctx.ports.blob.clone()?,
        wiki.kms.clone(),
        ctx.ports.clock.clone()?,
        ctx.ports.id_gen.clone()?,
    ))
}

/// A compiler over a context's ports, or `None` when a port is unwired.
pub(crate) fn compiler(ctx: &ModuleContext, wiki: &Wiki) -> Option<Compiler> {
    let db = ctx.ports.db.clone()?;
    let blob = ctx.ports.blob.clone()?;
    let clock = ctx.ports.clock.clone()?;
    let id_gen = ctx.ports.id_gen.clone()?;
    let http = ctx.ports.http.clone()?;
    let pages = livingbrain_pages::PageStore::new(
        db.clone(),
        blob.clone(),
        wiki.kms.clone(),
        clock.clone(),
        id_gen.clone(),
    );
    let sources = livingbrain_pages::SourceStore::new(
        db.clone(),
        blob,
        wiki.kms.clone(),
        clock.clone(),
        id_gen.clone(),
    );
    let facts = Arc::new(StoreFacts::new(FactStore::new(
        db.clone(),
        wiki.kms.clone(),
        clock.clone(),
        id_gen.clone(),
    )));
    let model = Arc::new(WorkspaceModel {
        db: db.clone(),
        http,
        config: Arc::clone(&ctx.config),
        clock: clock.clone(),
        id_gen,
    });
    let gate = Arc::new(UsageGate {
        db: db.clone(),
        config: Arc::clone(&ctx.config),
        clock: clock.clone(),
    });
    let dreams: Arc<dyn Dreams> = facts.clone();
    Some(
        Compiler::new(
            pages,
            sources,
            Queue::new(db.clone()),
            model,
            facts,
            gate,
            clock,
            Settings::from_config(&*ctx.config),
        )
        .with_dreams(Some(dreams))
        .with_tuning(Arc::new(StoredTuning { db: db.clone() }))
        .with_classifiers(wiki.jev.clone().map(|connector| {
            Arc::new(WorkspaceJev {
                db,
                config: Arc::clone(&ctx.config),
                connector,
            }) as Arc<dyn Classifiers>
        })),
    )
}

/// Each workspace's own Jev ([`livingbrain_models::jev::effective`]): its
/// switch, its own TypeSafe key, else the deployment's, else none. A read
/// that fails is no classifier — the free rule triages alone.
struct WorkspaceJev {
    db: Arc<dyn Database>,
    config: Arc<dyn Config>,
    connector: Arc<dyn livingbrain_models::jev::JevConnector>,
}

#[async_trait::async_trait]
impl Classifiers for WorkspaceJev {
    async fn for_workspace(&self, workspace: &str) -> Option<Arc<dyn cratefield_core::Classifier>> {
        livingbrain_models::jev::effective(&*self.db, &*self.config, workspace, &*self.connector)
            .await
            .ok()
            .and_then(|effective| effective.classifier)
    }
}

/// The most dream passes one cron tick runs: a pass is a model call, and a
/// tick is shared with the compile drain.
const DREAM_PASSES_PER_TICK: usize = 2;

/// Who the fact store records as the writer of a compiled fact: the
/// compiler's own marker, which the dreaming rule pass checks before it
/// retracts anything.
pub const FACT_WRITER: &str = livingbrain_compiler::COMPILER_WRITER;

/// The compiler's [`FactSink`] over the temporal facts store
/// (`docs/temporal-facts.md`): each fact is recorded in the scope the
/// session's source is in — the person's own — and nowhere else. A fact
/// the store refuses (a cardinality conflict, a field redaction will not
/// let through, anything outside the format) is that fact's skip, with a
/// reason the coding-agents log names; the rest of the batch is still
/// recorded and the job is never failed for it.
pub struct StoreFacts {
    store: FactStore,
}

impl StoreFacts {
    #[must_use]
    pub fn new(store: FactStore) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl FactSink for StoreFacts {
    async fn held(&self, scope: &str, subjects: &[String]) -> Result<Vec<HeldFact>, String> {
        let mut held = Vec::new();
        for subject in subjects {
            let timelines = self
                .store
                .timelines(&[scope], Some(subject), None)
                .await
                .map_err(|_| "store error".to_owned())?;
            for timeline in &timelines {
                held.extend(
                    timeline
                        .facts
                        .iter()
                        .filter(|fact| fact.status == FactStatus::Current)
                        .map(held_fact),
                );
            }
        }
        Ok(held)
    }

    async fn end(&self, scope: &str, id: &str, valid_to: &str) -> Result<bool, String> {
        match self.store.end(scope, id, valid_to).await {
            Ok(found) => Ok(found.is_some()),
            Err(FactError::Invalid(message)) => Err(format!("refused: {message}")),
            Err(_) => Err("store error".to_owned()),
        }
    }

    async fn retract(&self, scope: &str, id: &str) -> Result<bool, String> {
        match self.store.retract(&[scope], id).await {
            Ok(found) => Ok(found.is_some()),
            Err(_) => Err("store error".to_owned()),
        }
    }

    async fn emit(&self, _workspace: &str, scope: &str, facts: &[Fact]) -> Result<Emitted, String> {
        let mut emitted = Emitted::default();
        for fact in facts {
            let input = FactInput {
                subject: fact.subject.clone(),
                attribute: fact.attribute.clone(),
                value: fact.value.clone(),
                unit: fact.unit.clone(),
                valid_from: fact.valid_from.clone(),
                valid_to: fact.valid_to.clone(),
                observed_at: fact.observed_at.clone(),
                source: fact.source.clone(),
                quote: fact.quote.clone(),
                confidence: fact.confidence,
                cardinality: fact.cardinality.clone(),
            };
            match self.store.record(scope, input, FACT_WRITER).await {
                Ok(recorded) => {
                    emitted.recorded += 1;
                    if recorded.outcome == FactOutcome::Created {
                        emitted.created.push(recorded.fact.id);
                    }
                }
                Err(error) => emitted.skipped.push(skip_reason(fact, &error)),
            }
        }
        Ok(emitted)
    }
}

/// A current fact as the compiler reads it back ([`FactSink::held`]).
fn held_fact(fact: &TimedFact) -> HeldFact {
    HeldFact {
        id: fact.id.clone(),
        subject: fact.subject.clone(),
        attribute: fact.attribute.clone(),
        value: fact.value.clone(),
        unit: fact.unit.clone(),
        valid_from: fact.valid_from.text().to_owned(),
        valid_to: fact.valid_to.as_ref().map(|to| to.text().to_owned()),
        source: fact.source.clone(),
        observed_at: timeline::rfc3339(fact.observed_at),
        last_confirmed_at: timeline::rfc3339(fact.last_confirmed_at),
    }
}

/// Dreaming's bookkeeping, over the same store the facts are in.
#[async_trait::async_trait]
impl Dreams for StoreFacts {
    async fn candidates(
        &self,
        min_facts: usize,
        limit: usize,
    ) -> Result<Vec<DreamCandidate>, String> {
        self.store
            .dream_candidates(min_facts, limit)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn enqueue(&self, scope: &str, subject: &str, now: i64) -> Result<(), String> {
        self.store
            .dream_enqueue(scope, subject, now)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn queued(&self, limit: usize) -> Result<Vec<(String, String)>, String> {
        self.store
            .dream_queued(limit)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn claim(
        &self,
        scope: &str,
        subject: &str,
        now: i64,
        lease_until: i64,
    ) -> Result<bool, String> {
        self.store
            .dream_claim(scope, subject, now, lease_until)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn finish(
        &self,
        scope: &str,
        subject: &str,
        outcome: DreamFinish<'_>,
        now: i64,
    ) -> Result<(), String> {
        self.store
            .dream_finish(scope, subject, &outcome, now)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn log(&self, row: DreamDecisionRow<'_>) -> Result<(), String> {
        self.store
            .dream_log(&row)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn rule_batch(
        &self,
        limit: usize,
        now: i64,
    ) -> Result<Vec<livingbrain_pages::RuleFact>, String> {
        self.store
            .rule_batch(limit, now)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn cursor(&self, scope: &str, subject: &str) -> Result<Option<String>, String> {
        self.store
            .dream_cursor(scope, subject)
            .await
            .map_err(|_| "store error".to_owned())
    }

    async fn window(
        &self,
        scope: &str,
        subject: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Window, String> {
        let window = self
            .store
            .dream_window(scope, subject, after, limit)
            .await
            .map_err(|_| "store error".to_owned())?;
        Ok(Window {
            facts: window.facts.iter().map(held_fact).collect(),
            more: window.more,
            cursor: window.cursor,
        })
    }

    async fn vocabulary(
        &self,
        scope: &str,
        subject: &str,
    ) -> Result<Vec<(String, i64, String)>, String> {
        self.store
            .dream_vocabulary(scope, subject)
            .await
            .map_err(|_| "store error".to_owned())
    }
}

/// A refusal, said safely for the log. The store's own messages name a
/// field, never its content, and a refused fact's subject or attribute may
/// be the very thing redaction objected to, so only a cardinality conflict
/// (which passed those checks) names its timeline. A store failure is
/// named, not detailed.
fn skip_reason(fact: &Fact, error: &FactError) -> String {
    match error {
        FactError::CardinalityMismatch { attribute, stored } => format!(
            "cardinality conflict: `{attribute}` of `{}` is `{}` in this memory",
            fact.subject,
            stored.as_str()
        ),
        FactError::Invalid(message) => format!("refused: {message}"),
        FactError::Store(_) => "store error".to_owned(),
    }
}

/// The session owner's model on [`MODEL_ROLE`]: their own runner when they
/// run their jobs on their device, else the workspace's connection.
struct WorkspaceModel {
    db: Arc<dyn Database>,
    http: Arc<dyn HttpClient>,
    config: Arc<dyn Config>,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

#[async_trait::async_trait]
impl Model for WorkspaceModel {
    async fn available(&self, workspace: &str, owner: &str) -> Result<bool, String> {
        jobs::available(&*self.db, workspace, owner, MODEL_ROLE)
            .await
            .map_err(|error| error.variant().to_owned())
    }

    async fn complete(
        &self,
        call: &Call<'_>,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<Reply, ModelFailure> {
        // The edit list's schema: where the provider serves a forced tool
        // call, the answer is held to it; elsewhere it is parsed from text.
        let schema = livingbrain_compiler::edits::schema();
        let request = ChatRequest {
            system,
            user,
            max_tokens,
            json: true,
            structured: Some(Structured {
                name: "apply_edits",
                description: "Apply this session's wiki edit list: the log summary, the page edits and the temporal facts.",
                schema: &schema,
            }),
            timeout: CALL_TIMEOUT,
        };
        let ports = jobs::Ports {
            db: &*self.db,
            http: &*self.http,
            config: &*self.config,
            clock: &*self.clock,
            id_gen: &*self.id_gen,
        };
        let job = JobCall {
            owner: call.owner,
            kind: "compile",
            ref_key: call.ref_key,
        };
        match jobs::dispatch(&ports, call.workspace, MODEL_ROLE, &job, &request).await {
            Ok(Dispatch::Reply(reply)) => Ok(Reply {
                text: reply.text,
                prompt_tokens: reply.prompt_tokens,
                completion_tokens: reply.completion_tokens,
                runner: reply.runner,
                truncated: reply.truncated,
            }),
            Ok(Dispatch::Pending { .. }) => Err(ModelFailure::Pending),
            Err(InvokeError::NoModel) => Err(ModelFailure::NoModel),
            Err(error) if error.rate_limited() => Err(ModelFailure::Busy {
                reason: error.reason(),
                retry_after_secs: error.retry_after_secs(),
            }),
            Err(error) => Err(ModelFailure::Failed(error.reason())),
        }
    }
}

/// The usage ledger's verdict (`USAGE_BUDGET_CENTS_MONTHLY`), read the way
/// `GET /v1/usage/daily` reads it: month-to-date attributed cost — model
/// tokens priced by the serving connection, plus mail — against the
/// budget. A workspace over it compiles nothing until the month turns.
struct UsageGate {
    db: Arc<dyn Database>,
    config: Arc<dyn Config>,
    clock: Arc<dyn Clock>,
}

#[async_trait::async_trait]
impl Gate for UsageGate {
    async fn over_budget(&self, workspace: &str) -> bool {
        let serving = invoke::serving(&*self.db, workspace, MODEL_ROLE)
            .await
            .ok()
            .flatten();
        over_budget(
            &*self.db,
            &*self.config,
            &*self.clock,
            workspace,
            serving.as_ref(),
        )
        .await
    }
}

/// The month's attributed spend for `workspace`, in micro-USD, read the way
/// `GET /v1/usage/daily` reads it: month-to-date model tokens priced by
/// `serving` (the connection that would run the call), plus mail. The bool
/// says whether the model's tokens could be priced at all (an unpriced or
/// absent model counts zero, which is unknown rather than free). `None`
/// when the ledger cannot be read.
pub(crate) async fn month_spend_micro_usd(
    db: &dyn Database,
    clock: &dyn Clock,
    workspace: &str,
    serving: Option<&invoke::Serving>,
) -> Option<(u64, bool)> {
    let now = clock.now();
    let today = livingbrain_usage::utc_day(now);
    let first = format!("{}-01", &today[..7]);
    let rows = livingbrain_usage::days(db, workspace, &first, &today)
        .await
        .ok()?;
    let mtd = livingbrain_usage::month_to_date(&rows, now);
    let model = serving.and_then(|serving| {
        livingbrain_usage::model_cost_micro_usd(
            &serving.provider,
            &serving.model,
            mtd.prompt_tokens,
            mtd.completion_tokens,
        )
    });
    let priced = model.is_some();
    Some((
        model.unwrap_or(0) + livingbrain_usage::email_cost_micro_usd(mtd.emails_sent),
        priced,
    ))
}

/// Whether `workspace` is past its monthly spend limit — its own (Settings
/// → Usage & limits), else `USAGE_BUDGET_CENTS_MONTHLY`, else the default —
/// counting [`month_spend_micro_usd`]. The one rule both the compiler's gate
/// and `POST /v1/ask` answer to. A ledger that cannot be read is not over —
/// a lost read must not stop the brain.
pub(crate) async fn over_budget(
    db: &dyn Database,
    config: &dyn Config,
    clock: &dyn Clock,
    workspace: &str,
    serving: Option<&invoke::Serving>,
) -> bool {
    let Some((total, _)) = month_spend_micro_usd(db, clock, workspace, serving).await else {
        return false;
    };
    let own = livingbrain_usage::budgets_or_default(db, workspace).await;
    let budget = livingbrain_usage::budget_cents(config, &own);
    livingbrain_usage::status(total, budget) == livingbrain_usage::BudgetStatus::Over
}

/// `GET /queue` — the asker's jobs and where the queue stands.
async fn queue(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::GET, &headers).await?;
    Service::member_only(&asker)?;
    let (scopes, _) = job_scopes(&service.access(&asker).await?);
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let limit = query
        .get("limit")
        .and_then(|raw| raw.parse::<usize>().ok())
        .unwrap_or(50)
        .clamp(1, MAX_LIST);
    let source = query
        .get("source")
        .map(String::as_str)
        .filter(|id| !id.is_empty());
    if let Some(id) = source
        && (id.len() > 64 || !id.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return Err(Problem::validation_failed("source must be a source id"));
    }
    let now = clock.now();
    let queue = Queue::new(db.clone());
    queue
        .enqueue_from_ledger(now, Some(&asker.workspace_id))
        .await
        .map_err(|_| Problem::internal())?;
    let jobs = queue
        .list(&asker.workspace_id, &scopes, source, limit)
        .await
        .map_err(|_| Problem::internal())?;
    let counts = queue
        .counts(&asker.workspace_id, &scopes)
        .await
        .map_err(|_| Problem::internal())?;
    let savings = queue
        .savings(&asker.workspace_id, &scopes)
        .await
        .map_err(|_| Problem::internal())?;
    // The workspace's effective settings: its own, else the env vars.
    let own = livingbrain_usage::budgets_or_default(&*db, &asker.workspace_id).await;
    let settings = crate::budgets::compiler_settings(&*ctx.config, &own);
    let today = livingbrain_compiler::queue::utc_day(now);
    let first = format!("{}-01", &today[..7]);
    let spent_today = queue
        .spent(&asker.workspace_id, &today, &today)
        .await
        .map_err(|_| Problem::internal())?;
    let spent_month = queue
        .spent(&asker.workspace_id, &first, &today)
        .await
        .map_err(|_| Problem::internal())?;
    let serving = invoke::serving(&*db, &asker.workspace_id, MODEL_ROLE)
        .await
        .map_err(|_| Problem::internal())?;
    // The asker's own runner serves their compiles ahead of the workspace's
    // connection, so it is what the card names.
    let runner = jobs::runner_for(&*db, &asker.workspace_id, &asker.user_id, MODEL_ROLE)
        .await
        .map_err(|_| Problem::internal())?;
    // Where this workspace's Jev runs from now: `off`, `deployment` or
    // `workspace` (Settings → Models → Jev).
    let jev = match service.wiki().jev {
        Some(connector) => {
            livingbrain_models::jev::effective(&*db, &*ctx.config, &asker.workspace_id, &*connector)
                .await
                .map_err(|_| Problem::internal())?
                .source
        }
        None => livingbrain_models::jev::Source::Off,
    };
    Ok(Json(json!({
        "counts": counts
            .iter()
            .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
            .collect::<serde_json::Map<String, Value>>(),
        "model": match (runner, serving) {
            (Some(runner), _) => json!({
                "connected": true,
                "role": runner.role,
                "provider": format!("local-runner:{}", runner.agent),
                "model": runner.model,
                "runner": true,
                "runner_last_seen_at": runner.last_seen_at,
            }),
            (None, Some(serving)) => json!({
                "connected": true,
                "role": serving.role,
                "provider": serving.provider,
                "model": serving.model,
            }),
            (None, None) => json!({"connected": false, "role": MODEL_ROLE}),
        },
        "budget": {
            "paused": settings.paused,
            "batch": settings.batch,
            "concurrency": settings.concurrency,
            "daily_tokens": settings.daily_tokens,
            "monthly_tokens": settings.monthly_tokens,
            "job_tokens": settings.job_tokens,
            "spent_today": spent_today,
            "spent_month": spent_month,
        },
        "triage": {
            "enabled": settings.triage,
            "compaction": settings.compaction,
            "strictness": settings.strictness.as_str(),
            "classifier": jev != livingbrain_models::jev::Source::Off,
            "classifier_source": jev.as_str(),
            "skipped": savings.skipped,
            "summarised": savings.summarised,
            "full": savings.full,
            "tokens_raw": savings.tokens_raw,
            "tokens_sent": savings.tokens_sent,
            "tokens_saved": savings.tokens_saved(),
        },
        "jobs": jobs.iter().map(job_view).collect::<Vec<_>>(),
    })))
}

/// `POST /process` — compile up to `max` of the asker's jobs now.
async fn process(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::POST, &headers).await?;
    let max = if body.is_empty() {
        1
    } else {
        let value: Value = serde_json::from_slice(&body)
            .map_err(|_| Problem::validation_failed("the body is not JSON"))?;
        value
            .get("max")
            .and_then(Value::as_u64)
            .map_or(1, |max| usize::try_from(max).unwrap_or(1))
            .clamp(1, MAX_PROCESS_NOW)
    };
    Service::member_only(&asker)?;
    let (scopes, own) = job_scopes(&service.access(&asker).await?);
    let compiler = compiler(service.ctx(), &service.wiki()).ok_or_else(Problem::internal)?;
    let filter = ClaimFilter {
        workspace: Some(asker.workspace_id.clone()),
        scopes: Some(own),
        ignore_backoff: true,
        source_id: None,
    };
    let processed = compiler
        .drain(&filter, max, &ScheduledBudget::unbounded())
        .await
        .map_err(|_| Problem::internal())?;
    let counts = compiler
        .queue()
        .counts(&asker.workspace_id, &scopes)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(Json(json!({
        "processed": processed.iter().map(|p| json!({
            "source_id": p.source_id,
            "status": p.status.as_str(),
            "pages": p.pages,
            "error": p.error,
        })).collect::<Vec<_>>(),
        "counts": counts
            .iter()
            .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
            .collect::<serde_json::Map<String, Value>>(),
    })))
}

/// `POST /compile-fully` — compile one of the asker's triaged sessions in
/// full: no triage, no compaction, now.
async fn compile_fully(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::POST, &headers).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| Problem::validation_failed("the body is not JSON"))?;
    let source_id = value
        .get("source_id")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
        .ok_or_else(|| Problem::validation_failed("source_id must be a source id"))?
        .to_owned();
    Service::member_only(&asker)?;
    // The asker's own memory only, as for `Process now`.
    let (scopes, own) = job_scopes(&service.access(&asker).await?);
    let ctx = service.ctx();
    let clock = port(ctx.ports.clock.clone())?;
    let compiler = compiler(ctx, &service.wiki()).ok_or_else(Problem::internal)?;
    let requeued = compiler
        .queue()
        .compile_fully(&asker.workspace_id, &own, &source_id, clock.now())
        .await
        .map_err(|_| Problem::internal())?;
    if !requeued {
        return Err(Problem::not_found()
            .with_detail("no skipped-trivial or summary-only job of yours has that source id"));
    }
    let filter = ClaimFilter {
        workspace: Some(asker.workspace_id.clone()),
        scopes: Some(own),
        ignore_backoff: true,
        source_id: Some(source_id),
    };
    let processed = compiler
        .drain(&filter, 1, &ScheduledBudget::unbounded())
        .await
        .map_err(|_| Problem::internal())?;
    let counts = compiler
        .queue()
        .counts(&asker.workspace_id, &scopes)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(Json(json!({
        "requeued": true,
        "processed": processed.iter().map(|p| json!({
            "source_id": p.source_id,
            "status": p.status.as_str(),
            "pages": p.pages,
            "error": p.error,
        })).collect::<Vec<_>>(),
        "counts": counts
            .iter()
            .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
            .collect::<serde_json::Map<String, Value>>(),
    })))
}

/// `POST /retry` — requeue the asker's own failed jobs.
async fn retry(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::POST, &headers).await?;
    Service::member_only(&asker)?;
    // The asker's own memory only, as for `Process now`.
    let (scopes, own) = job_scopes(&service.access(&asker).await?);
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let queue = Queue::new(db);
    let requeued = queue
        .requeue_failed(&asker.workspace_id, &own, clock.now())
        .await
        .map_err(|_| Problem::internal())?;
    let counts = queue
        .counts(&asker.workspace_id, &scopes)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(Json(json!({
        "requeued": requeued,
        "counts": counts
            .iter()
            .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
            .collect::<serde_json::Map<String, Value>>(),
    })))
}

/// The scopes a member's compile jobs are listed from, and the ones their
/// own say-so may compile (ADR 0004).
///
/// A job is about a source, so it is listed to whoever may read that
/// source's whole scope. Only the asker's own memory is compiled on their
/// say-so — a shared session (if one ever exists) is the cron's to drain, on
/// the workspace's schedule, not any one member's button — and only where
/// the credential may write: a token narrowed to reading cannot start a
/// compile that writes pages.
pub(super) fn job_scopes(plan: &AccessPlan) -> (Vec<String>, Vec<String>) {
    let scopes: Vec<String> = plan
        .filters(KIND_SOURCES, Permission::Read)
        .into_iter()
        .filter(ScopeFilter::is_whole)
        .map(|filter| filter.scope)
        .collect();
    let own = plan
        .own_scope()
        .filter(|own| scopes.contains(own))
        .filter(|own| {
            plan.filters(KIND_PAGES, Permission::Write)
                .iter()
                .any(|filter| &filter.scope == own && filter.is_whole())
        })
        .into_iter()
        .collect();
    (scopes, own)
}

/// One job as the routes answer it. No scope hash, no body hash: the
/// source id is the handle the rest of the API already uses.
fn job_view(job: &Job) -> Value {
    json!({
        "source_id": job.source_id,
        "session": job.session_key,
        "project": job.project,
        "status": job.status.as_str(),
        "attempts": job.attempts,
        "error": job.last_error,
        "pages": job.pages,
        "rejected": job.rejected,
        "facts": job.facts,
        "tokens": {"prompt": job.prompt_tokens, "completion": job.completion_tokens},
        "queued_at": job.queued_at,
        "updated_at": job.updated_at,
        "next_attempt_at": job.next_attempt_at,
        "triage": job.triage.as_ref().map(|decision| json!({
            "decision": decision,
            "reason": job.triage_reason,
            "forced": job.force_full,
        })),
        "estimate": {"raw": job.est_tokens_raw, "sent": job.est_tokens_sent},
        "can_compile_fully": matches!(
            job.status,
            JobStatus::SkippedTrivial | JobStatus::SummaryOnly
        ),
    })
}
