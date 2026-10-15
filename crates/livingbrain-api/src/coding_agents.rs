//! The `coding-agents` module: the compiler's queue, its two routes, and
//! its cron drain (see `livingbrain-compiler` for what compiling is).
//!
//! - `GET /v1/coding-agents/queue?limit=&source=` — the asker's jobs, newest
//!   first, with a count per status, whether a model is connected, and the
//!   compile budget and what is spent of it. A read first enqueues whatever
//!   the ledger holds that has no job yet, so a session synced a moment ago
//!   shows as `queued` rather than as nothing.
//! - `POST /v1/coding-agents/process` (`{"max": 1..3}`) — compiles up to
//!   `max` of the asker's own jobs now, in the request, and answers what
//!   became of each. A parked job (`skipped-no-model`, `over-budget`) is
//!   re-checked at once, since the person pressing the button may just have
//!   connected a model. Cookie writes must be same-origin, like every cookie
//!   write on this surface.
//!
//! Both answer only for the asker's own scopes — the same two clauses the
//! source listing applies — so a member sees and spends nothing of anybody
//! else's. The cron tick (`scheduled`) drains every workspace in bounded
//! batches (`CODING_AGENTS_BATCH`) under the invocation's cooperative
//! budget, then runs the model-free lint over the scopes it compiled into.
//!
//! The model is the workspace's own, through `livingbrain_models::invoke`,
//! on the **Main** job slot: there is no managed fallback, so a workspace
//! with nothing connected parks its jobs as `skipped-no-model` and the
//! status route says so.

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
    Migrations, Module, ModuleContext, PersonalDataSet, Port, Problem, ScheduledBudget,
    SqlMigration,
};
use livingbrain_compiler::{
    ClaimFilter, Compiler, Emitted, Fact, FactSink, Gate, Job, JobStatus, Model, ModelFailure,
    Queue, Reply, Settings,
};
use livingbrain_models::invoke::{self, ChatRequest, InvokeError};
use livingbrain_pages::{FactError, FactInput, FactStore};
use serde_json::{Value, json};

use crate::Wiki;
use crate::port;
use crate::service::Service;

/// The job slot a compile runs on. Main, because the compile is the
/// workspace's main writing job; a role of its own is a later change.
pub const MODEL_ROLE: &str = "main";

/// The most jobs one `Process now` compiles: each is one to a few model
/// calls inside the request.
const MAX_PROCESS_NOW: usize = 3;
/// How long one compile call may take.
const CALL_TIMEOUT: StdDuration = StdDuration::from_secs(120);
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
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const SQLITE: [SqlMigration; 1] = [livingbrain_compiler::MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&SQLITE);
        const POSTGRES: [SqlMigration; 1] = [livingbrain_compiler::MIGRATION_INIT_POSTGRES];
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
            let settings = Settings::from_config(&*ctx.config);
            let processed = compiler
                .drain(&ClaimFilter::default(), settings.batch, &ctx.scheduled)
                .await
                .map_err(|error| Box::new(error) as AnyError)?;
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
fn compiler(ctx: &ModuleContext, wiki: &Wiki) -> Option<Compiler> {
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
        id_gen,
    )));
    let model = Arc::new(WorkspaceModel {
        db: db.clone(),
        http,
        config: Arc::clone(&ctx.config),
        clock: clock.clone(),
    });
    let gate = Arc::new(UsageGate {
        db: db.clone(),
        config: Arc::clone(&ctx.config),
        clock: clock.clone(),
    });
    Some(Compiler::new(
        pages,
        sources,
        Queue::new(db),
        model,
        facts,
        gate,
        clock,
        Settings::from_config(&*ctx.config),
    ))
}

/// Who the fact store records as the writer of a compiled fact.
pub const FACT_WRITER: &str = "brain:coding-agents";

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
                Ok(_) => emitted.recorded += 1,
                Err(error) => emitted.skipped.push(skip_reason(fact, &error)),
            }
        }
        Ok(emitted)
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

/// The workspace's own connected model, on [`MODEL_ROLE`].
struct WorkspaceModel {
    db: Arc<dyn Database>,
    http: Arc<dyn HttpClient>,
    config: Arc<dyn Config>,
    clock: Arc<dyn Clock>,
}

#[async_trait::async_trait]
impl Model for WorkspaceModel {
    async fn available(&self, workspace: &str) -> Result<bool, String> {
        invoke::serving(&*self.db, workspace, MODEL_ROLE)
            .await
            .map(|serving| serving.is_some())
            .map_err(|error| error.variant().to_owned())
    }

    async fn complete(
        &self,
        workspace: &str,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<Reply, ModelFailure> {
        let request = ChatRequest {
            system,
            user,
            max_tokens,
            json: true,
            timeout: CALL_TIMEOUT,
        };
        match invoke::chat(
            &*self.db,
            &*self.http,
            &*self.config,
            &*self.clock,
            workspace,
            MODEL_ROLE,
            &request,
        )
        .await
        {
            Ok(reply) => Ok(Reply {
                text: reply.text,
                prompt_tokens: reply.prompt_tokens,
                completion_tokens: reply.completion_tokens,
            }),
            Err(InvokeError::NoModel) => Err(ModelFailure::NoModel),
            Err(error) => Err(ModelFailure::Failed(error.variant().to_owned())),
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
        let now = self.clock.now();
        let today = livingbrain_usage::utc_day(now);
        let first = format!("{}-01", &today[..7]);
        let Ok(rows) = livingbrain_usage::days(&*self.db, workspace, &first, &today).await else {
            return false;
        };
        let mtd = livingbrain_usage::month_to_date(&rows, now);
        let model = match invoke::serving(&*self.db, workspace, MODEL_ROLE).await {
            Ok(Some(serving)) => livingbrain_usage::model_cost_micro_usd(
                &serving.provider,
                &serving.model,
                mtd.prompt_tokens,
                mtd.completion_tokens,
            )
            .unwrap_or(0),
            _ => 0,
        };
        let total = model + livingbrain_usage::email_cost_micro_usd(mtd.emails_sent);
        let budget = self
            .config
            .get("USAGE_BUDGET_CENTS_MONTHLY")
            .and_then(|raw| raw.trim().parse::<u64>().ok())
            .unwrap_or(livingbrain_usage::DEFAULT_BUDGET_CENTS_MONTHLY);
        livingbrain_usage::status(total, budget) == livingbrain_usage::BudgetStatus::Over
    }
}

/// `GET /queue` — the asker's jobs and where the queue stands.
async fn queue(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
    Query(query): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::GET, &headers).await?;
    let scopes = service.scopes(&asker);
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
    let settings = Settings::from_config(&*ctx.config);
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
    Ok(Json(json!({
        "counts": counts
            .iter()
            .map(|(status, n)| (status.as_str().to_owned(), json!(n)))
            .collect::<serde_json::Map<String, Value>>(),
        "model": match serving {
            Some(serving) => json!({
                "connected": true,
                "role": serving.role,
                "provider": serving.provider,
                "model": serving.model,
            }),
            None => json!({"connected": false, "role": MODEL_ROLE}),
        },
        "budget": {
            "daily_tokens": settings.daily_tokens,
            "monthly_tokens": settings.monthly_tokens,
            "job_tokens": settings.job_tokens,
            "spent_today": spent_today,
            "spent_month": spent_month,
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
    let scopes = service.scopes(&asker);
    // Only the asker's own memory is compiled on their say-so: a shared
    // session (if one ever exists) is the cron's to drain, on the
    // workspace's schedule, not any one member's button.
    let own: Vec<String> = scopes
        .iter()
        .filter(|scope| scope.starts_with("user-"))
        .cloned()
        .collect();
    let compiler = compiler(service.ctx(), &service.wiki()).ok_or_else(Problem::internal)?;
    let filter = ClaimFilter {
        workspace: Some(asker.workspace_id.clone()),
        scopes: Some(own),
        ignore_backoff: true,
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
    })
}
