//! The compiler: drains the queue, one session at a time, into the
//! `coding-agents/` pages of the session's own scope.
//!
//! For each job: open the source (sealed, already redacted at ingest),
//! parse it version-tolerantly, read the person's conventions and the pages
//! the session may touch, and hand the model one transcript chunk at a time
//! — every input redacted again on the way out, every output redacted on the
//! way in. The model's answer is only ever a closed edit list
//! ([`crate::edits`]); each edit is checked against the session's folder
//! ([`crate::layout::Allowed`]) and written with the page store's optimistic
//! concurrency as the brain. The server appends the log line itself.
//!
//! **Scope.** A source lives in one page scope — a person's own memory, for
//! every synced session — and its compiled pages are written into that same
//! scope and no other. Nothing here takes a scope from a model or a client:
//! the job carries the source's scope, and the store's scope rule does the
//! rest, so one person's sessions can never compile into another person's
//! pages or into the shared wiki.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::{Clock, Config, DbError, ScheduledBudget};
use livingbrain_pages::{
    Author, EntityType, Page, PageError, PageStore, PageWrite, SourceError, SourceStore,
};
use livingbrain_redact::{Policy, redact};
use time::{Duration, OffsetDateTime};

use crate::edits::{self, PageEdit};
use crate::facts::{Fact, FactSink};
use crate::layout::{self, Allowed, Place};
use crate::prompt::{self, SessionHead, Shown};
use crate::queue::{ClaimFilter, Finish, Job, JobStatus, MAX_ATTEMPTS, Queue, utc_day};
use crate::record;

/// The knobs, all from config (`CODING_AGENTS_*`), all with defaults that
/// keep one session's compile small and a workspace's month bounded.
#[derive(Debug, Clone)]
pub struct Settings {
    /// Jobs one cron tick compiles (`CODING_AGENTS_BATCH`, default 1).
    pub batch: usize,
    /// Transcript characters per model call (`CODING_AGENTS_CHUNK_CHARS`,
    /// default 32 000 — about 8 000 tokens).
    pub chunk_chars: usize,
    /// Calls one session may take (`CODING_AGENTS_MAX_CHUNKS`, default 4);
    /// the rest of a longer transcript is left out and the log says so.
    pub max_chunks: usize,
    /// Characters of existing pages shown per call
    /// (`CODING_AGENTS_CONTEXT_CHARS`, default 32 000). A page past it is
    /// shown cut and is read-only for that call.
    pub context_chars: usize,
    /// The completion ceiling per call (`CODING_AGENTS_MAX_OUTPUT_TOKENS`,
    /// default 6 000).
    pub max_output_tokens: u32,
    /// The hard cap on one job's tokens, in and out, across its calls
    /// (`CODING_AGENTS_JOB_TOKENS`, default 80 000).
    pub job_tokens: u64,
    /// A workspace's compile tokens per UTC day
    /// (`CODING_AGENTS_DAILY_TOKENS`, default 400 000).
    pub daily_tokens: u64,
    /// A workspace's compile tokens per UTC month
    /// (`CODING_AGENTS_MONTHLY_TOKENS`, default 4 000 000).
    pub monthly_tokens: u64,
    /// How long a claim holds a job before another drain may take it.
    pub lease: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            batch: 1,
            chunk_chars: 32_000,
            max_chunks: 4,
            context_chars: 32_000,
            max_output_tokens: 6_000,
            job_tokens: 80_000,
            daily_tokens: 400_000,
            monthly_tokens: 4_000_000,
            lease: Duration::minutes(15),
        }
    }
}

impl Settings {
    /// The settings from `CODING_AGENTS_*` config keys; an unset or
    /// unparsable key keeps its default.
    #[must_use]
    pub fn from_config(config: &dyn Config) -> Self {
        let default = Self::default();
        let number = |key: &str, fallback: u64| {
            config
                .get(key)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .unwrap_or(fallback)
        };
        let size = |key: &str, fallback: usize| {
            usize::try_from(number(key, fallback as u64)).unwrap_or(fallback)
        };
        Self {
            batch: size("CODING_AGENTS_BATCH", default.batch).clamp(1, 20),
            chunk_chars: size("CODING_AGENTS_CHUNK_CHARS", default.chunk_chars).max(1_000),
            max_chunks: size("CODING_AGENTS_MAX_CHUNKS", default.max_chunks).clamp(1, 20),
            context_chars: size("CODING_AGENTS_CONTEXT_CHARS", default.context_chars).max(1_000),
            max_output_tokens: u32::try_from(number(
                "CODING_AGENTS_MAX_OUTPUT_TOKENS",
                u64::from(default.max_output_tokens),
            ))
            .unwrap_or(default.max_output_tokens)
            .max(256),
            job_tokens: number("CODING_AGENTS_JOB_TOKENS", default.job_tokens),
            daily_tokens: number("CODING_AGENTS_DAILY_TOKENS", default.daily_tokens),
            monthly_tokens: number("CODING_AGENTS_MONTHLY_TOKENS", default.monthly_tokens),
            lease: default.lease,
        }
    }
}

/// One model call's answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reply {
    pub text: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// The owner's own runner answered, on their subscription: the tokens
    /// are theirs, not the workspace's compile budget.
    pub runner: bool,
}

/// Whose call a completion is, and the compile's handle for it: a model
/// that hands calls to the owner's own runner needs both to route the call
/// and to find its answer again.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    pub workspace: &'a str,
    /// The member whose session this is.
    pub owner: &'a str,
    /// The compile job and chunk (`<scope>:<body_sha256>:<chunk>`).
    pub ref_key: &'a str,
}

/// Why a model call did not answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelFailure {
    /// No model is connected in the workspace.
    NoModel,
    /// The call failed; the string is a short fixed-vocabulary reason.
    Failed(String),
    /// The call was handed to the owner's own runner; its answer arrives
    /// later, and the compile waits for it.
    Pending,
}

/// The workspace's model, as the compiler sees it. The composition wires
/// it over `livingbrain_models::invoke`; the tests over a fake.
#[async_trait::async_trait]
pub trait Model: Send + Sync {
    /// Whether a model would serve `owner`'s compile in `workspace` — their
    /// own runner or the workspace's connection. No call made.
    async fn available(&self, workspace: &str, owner: &str) -> Result<bool, String>;
    /// One completion.
    async fn complete(
        &self,
        call: &Call<'_>,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<Reply, ModelFailure>;
}

/// A second budget opinion beside the compiler's own token caps: the
/// composition wires one over the workspace's usage ledger
/// (`USAGE_BUDGET_CENTS_MONTHLY`), so a workspace already over its infra
/// budget compiles nothing.
#[async_trait::async_trait]
pub trait Gate: Send + Sync {
    async fn over_budget(&self, workspace: &str) -> bool;
}

/// No second opinion.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoGate;

#[async_trait::async_trait]
impl Gate for NoGate {
    async fn over_budget(&self, _workspace: &str) -> bool {
        false
    }
}

/// One job a drain looked at, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Processed {
    pub source_id: String,
    pub status: JobStatus,
    pub pages: Vec<String>,
    pub error: Option<String>,
}

/// The compiler over one request's (or one cron tick's) ports.
pub struct Compiler {
    pages: PageStore,
    sources: SourceStore,
    queue: Queue,
    model: Arc<dyn Model>,
    facts: Arc<dyn FactSink>,
    gate: Arc<dyn Gate>,
    clock: Arc<dyn Clock>,
    settings: Settings,
}

/// What one job accumulated across its calls.
#[derive(Debug, Default)]
struct Tally {
    prompt: u64,
    completion: u64,
    pages: Vec<String>,
    rejected: u32,
    conflicts: u32,
    facts: u32,
    /// One reason per fact the store refused, in order.
    skipped_facts: Vec<String>,
    /// Facts the sink could not try at all, and why.
    unrecorded_facts: Option<(u32, String)>,
    summaries: Vec<String>,
}

impl Tally {
    fn tokens(&self) -> u64 {
        self.prompt.saturating_add(self.completion)
    }

    fn written(&mut self, slug: &str) {
        if !self.pages.iter().any(|seen| seen == slug) {
            self.pages.push(slug.to_owned());
        }
    }
}

/// What one edit came to.
enum Applied {
    Written,
    Rejected,
    Conflict,
}

impl Compiler {
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        pages: PageStore,
        sources: SourceStore,
        queue: Queue,
        model: Arc<dyn Model>,
        facts: Arc<dyn FactSink>,
        gate: Arc<dyn Gate>,
        clock: Arc<dyn Clock>,
        settings: Settings,
    ) -> Self {
        Self {
            pages,
            sources,
            queue,
            model,
            facts,
            gate,
            clock,
            settings,
        }
    }

    /// The queue this compiler drains.
    #[must_use]
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Enqueues what the ledger holds, then compiles up to `max_jobs` jobs
    /// the filter allows, oldest first — parking the ones whose workspace
    /// has no model or no budget left without spending anything on them.
    /// `budget` is the scheduled invocation's cooperative budget (unbounded
    /// outside a cron tick); the drain stops when it runs out.
    ///
    /// # Errors
    ///
    /// [`DbError`] when the queue itself cannot be read or written. A
    /// failing job is a `failed` row, not an error here.
    pub async fn drain(
        &self,
        filter: &ClaimFilter,
        max_jobs: usize,
        budget: &ScheduledBudget,
    ) -> Result<Vec<Processed>, DbError> {
        let now = self.clock.now();
        self.queue
            .enqueue_from_ledger(now, filter.workspace.as_deref())
            .await?;
        let ready = self
            .queue
            .ready(now, filter, max_jobs.saturating_mul(4).max(4))
            .await?;
        let mut models: BTreeMap<(String, String), bool> = BTreeMap::new();
        let mut out = Vec::new();
        let mut compiled = 0usize;
        for job in ready {
            if compiled >= max_jobs || budget.exhausted(self.clock.now()) {
                break;
            }
            let now = self.clock.now();
            let who = (job.workspace.clone(), job.owner.clone());
            let available = match models.get(&who) {
                Some(known) => *known,
                None => {
                    let known = self
                        .model
                        .available(&job.workspace, &job.owner)
                        .await
                        .unwrap_or(false);
                    models.insert(who, known);
                    known
                }
            };
            if !available {
                if self
                    .queue
                    .park(
                        &job,
                        JobStatus::SkippedNoModel,
                        now,
                        now + Duration::hours(1),
                        "no-model",
                    )
                    .await?
                {
                    out.push(parked(&job, JobStatus::SkippedNoModel, "no-model"));
                }
                continue;
            }
            if let Some(until) = self.over_budget(&job.workspace, now).await? {
                if self
                    .queue
                    .park(&job, JobStatus::OverBudget, now, until, "over-budget")
                    .await?
                {
                    out.push(parked(&job, JobStatus::OverBudget, "over-budget"));
                }
                continue;
            }
            // A cron tick's share is counted in subrequest-shaped steps; a
            // compile is several (the source, the pages, the model calls).
            if !budget.try_spend(8) {
                break;
            }
            if !self.queue.claim(&job, now, self.settings.lease).await? {
                continue;
            }
            let claimed = Job {
                attempts: job.attempts + 1,
                status: JobStatus::Processing,
                ..job
            };
            let finish = self.process(&claimed).await;
            self.queue
                .finish(&claimed, &finish, self.clock.now())
                .await?;
            compiled += 1;
            out.push(Processed {
                source_id: claimed.source_id.clone(),
                status: finish.status,
                pages: finish.pages.clone(),
                error: finish.error.clone(),
            });
        }
        Ok(out)
    }

    /// `Some(when it may reset)` when the workspace may not compile now:
    /// its daily or monthly compile tokens are spent, or the usage gate
    /// says it is over its budget.
    async fn over_budget(
        &self,
        workspace: &str,
        now: OffsetDateTime,
    ) -> Result<Option<OffsetDateTime>, DbError> {
        let today = utc_day(now);
        let month_start = format!("{}-01", &today[..7]);
        let spent_today = self.queue.spent(workspace, &today, &today).await?;
        let spent_month = self.queue.spent(workspace, &month_start, &today).await?;
        let tomorrow = start_of_next_day(now);
        if spent_month >= self.settings.monthly_tokens {
            return Ok(Some(start_of_next_month(now)));
        }
        if spent_today >= self.settings.daily_tokens {
            return Ok(Some(tomorrow));
        }
        if self.gate.over_budget(workspace).await {
            return Ok(Some(now + Duration::hours(6)));
        }
        Ok(None)
    }

    /// Compiles one claimed job, and says how it ended. Never fails: every
    /// failure is a status the queue records.
    async fn process(&self, job: &Job) -> Finish {
        let now = self.clock.now();
        let source = match self.sources.by_id(&job.source_id).await {
            Ok(Some(source)) if source.scope == job.scope && source.workspace == job.workspace => {
                source
            }
            Ok(_) => return Finish::status(JobStatus::Skipped, Some("source-gone")),
            Err(_) => return self.failed(job, now, "store", Tally::default()),
        };
        let body = match self.sources.open(&source.scope, &source.body_sha256).await {
            Ok(body) => body,
            Err(SourceError::Shredded(_) | SourceError::Corrupt(_)) => {
                return Finish::status(JobStatus::Skipped, Some("source-gone"));
            }
            Err(_) => return self.failed(job, now, "source-unreadable", Tally::default()),
        };
        // `me` in a fact is the person who synced the session.
        let me = livingbrain_pages::person_subject(&source.imported_by);
        let Ok(session) = record::parse(&body) else {
            return Finish::status(JobStatus::Skipped, Some("not-a-session"));
        };
        let agent = layout::agent_segment(&session.agent);
        let project = layout::project_segment(
            job.project
                .as_deref()
                .or_else(|| session.repo.as_deref().and_then(basename))
                .unwrap_or("no-project"),
        );
        let allowed = Allowed {
            agent: agent.clone(),
            project: project.clone(),
        };
        if self.seed(&job.scope, now).await.is_err() {
            return self.failed(job, now, "store", Tally::default());
        }
        let conventions = match self.pages.read(&job.scope, layout::SCHEMA).await {
            Ok(Some(page)) => strip_frontmatter(&page.markdown),
            _ => prompt::DEFAULT_SCHEMA.to_owned(),
        };
        let head = SessionHead {
            agent: session.agent.clone(),
            agent_folder: agent,
            project,
            session_id: session.session_id.clone(),
            source_id: job.source_id.clone(),
            day: session.day().unwrap_or_else(|| utc_day(now)),
            branch: session.branch.clone(),
            models: session.models.clone(),
            totals: session.totals.clone(),
            previously_compiled: self.queue.previously_compiled(job).await.ok().flatten(),
        };
        let (chunks, mut omitted) =
            session.chunks(self.settings.chunk_chars, self.settings.max_chunks);
        if chunks.is_empty() || session.turns.is_empty() {
            return Finish::status(JobStatus::Skipped, Some("empty-session"));
        }

        let mut tally = Tally::default();
        let parts = chunks.len();
        // A compile that waited on the owner's runner resumes after the
        // chunks it already applied; their tokens are on the row already.
        let resumed = usize::try_from(job.chunks_done).unwrap_or(0).min(parts);
        let spent_before = job.prompt_tokens.saturating_add(job.completion_tokens);
        for (index, chunk) in chunks.iter().enumerate().skip(resumed) {
            let shown = match self.context(&job.scope, &allowed).await {
                Ok(shown) => shown,
                Err(_) => return self.failed(job, now, "store", tally),
            };
            let user = prompt::user_message(
                &conventions,
                &head,
                &shown,
                chunk,
                index + 1,
                parts,
                omitted,
            );
            // Redacted on the way out: the source was redacted at ingest,
            // but the pages and the conventions are text a person typed.
            let Ok((user, _)) = redact(&user, Policy::Redact) else {
                return self.failed(job, now, "not-redactable", tally);
            };
            let estimate = prompt::estimate_tokens(prompt::SYSTEM)
                + prompt::estimate_tokens(&user)
                + u64::from(self.settings.max_output_tokens);
            if spent_before
                .saturating_add(tally.tokens())
                .saturating_add(estimate)
                > self.settings.job_tokens
            {
                omitted = true;
                break;
            }
            let ref_key = format!("{}:{}:{index}", job.scope, job.body_sha256);
            let call = Call {
                workspace: &job.workspace,
                owner: &job.owner,
                ref_key: &ref_key,
            };
            let reply = match self
                .model
                .complete(
                    &call,
                    prompt::SYSTEM,
                    &user,
                    self.settings.max_output_tokens,
                )
                .await
            {
                Ok(reply) => reply,
                Err(ModelFailure::NoModel) => {
                    return Finish {
                        retry_at: Some(now + Duration::hours(1)),
                        ..self.with_tally(JobStatus::SkippedNoModel, Some("no-model"), tally)
                    };
                }
                Err(ModelFailure::Failed(reason)) => {
                    let reason = format!("model-{reason}");
                    return self.failed(job, now, &reason, tally);
                }
                // Out on the owner's runner: what this run applied stays, and
                // the job carries on from this chunk when the answer comes
                // back (at once, through the result route; an hour from now
                // at the latest, from the cron).
                Err(ModelFailure::Pending) => {
                    return Finish {
                        retry_at: Some(now + Duration::hours(1)),
                        chunks_done: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                        refund_attempt: true,
                        ..self.with_tally(JobStatus::WaitingRunner, Some("waiting-runner"), tally)
                    };
                }
            };
            tally.prompt = tally.prompt.saturating_add(reply.prompt_tokens);
            tally.completion = tally.completion.saturating_add(reply.completion_tokens);
            // A runner's tokens were the owner's subscription, not the
            // workspace's compile budget.
            if !reply.runner {
                let spent = reply.prompt_tokens + reply.completion_tokens;
                let _ = self
                    .queue
                    .record_spend(self.clock.now(), &job.workspace, spent)
                    .await;
            }

            let Ok(output) = edits::parse(&reply.text) else {
                tally.rejected += 1;
                if tally.pages.is_empty() {
                    return self.failed(job, now, "bad-output", tally);
                }
                break;
            };
            tally.rejected += output.refused;
            for edit in &output.pages {
                match self
                    .apply(&job.scope, &allowed, &shown, edit, &head, now)
                    .await
                {
                    Applied::Written => tally.written(&edit.slug),
                    Applied::Rejected => tally.rejected += 1,
                    Applied::Conflict => tally.conflicts += 1,
                }
            }
            if !output.facts.is_empty() {
                let (facts, refused): (Vec<Fact>, Vec<Fact>) = self
                    .prepare_facts(&output.facts, job, &head, me.as_deref(), now)
                    .into_iter()
                    .partition(crate::facts::valid);
                tally.skipped_facts.extend(
                    refused
                        .iter()
                        .map(|_| "outside the v1 format once redacted".to_owned()),
                );
                // A refusal is per fact, and a sink that cannot try at all
                // is a note in the log: neither fails the job.
                match self.facts.emit(&job.workspace, &job.scope, &facts).await {
                    Ok(emitted) => {
                        tally.facts = tally
                            .facts
                            .saturating_add(u32::try_from(emitted.recorded).unwrap_or(u32::MAX));
                        tally.skipped_facts.extend(emitted.skipped);
                    }
                    Err(reason) => {
                        let count = u32::try_from(facts.len()).unwrap_or(u32::MAX);
                        let (before, _) = tally.unrecorded_facts.take().unwrap_or_default();
                        tally.unrecorded_facts = Some((before.saturating_add(count), reason));
                    }
                }
            }
            if !output.summary.is_empty() {
                tally.summaries.push(redacted_line(&output.summary));
            }
        }

        if tally.pages.is_empty() && tally.conflicts > 0 {
            return self.failed(job, now, "conflict", tally);
        }
        let line = log_line(&head, &tally, omitted, now);
        if self.append_log(&job.scope, &line).await {
            tally.written(layout::LOG);
        }
        let error = match (tally.conflicts, tally.rejected) {
            (0, 0) => None,
            (conflicts, rejected) => {
                Some(format!("partial: {conflicts} stale, {rejected} refused"))
            }
        };
        Finish {
            chunks_done: Some(u32::try_from(parts).unwrap_or(u32::MAX)),
            ..self.with_tally(JobStatus::Done, error.as_deref(), tally)
        }
    }

    /// A `failed` finish, with the retry time its attempt count earns: 15
    /// minutes, then an hour, then none — the job stays failed.
    fn failed(&self, job: &Job, now: OffsetDateTime, reason: &str, tally: Tally) -> Finish {
        let retry_at = (job.attempts < MAX_ATTEMPTS)
            .then(|| now + Duration::minutes(15 * 4_i64.pow(job.attempts.saturating_sub(1))));
        Finish {
            retry_at,
            ..self.with_tally(JobStatus::Failed, Some(reason), tally)
        }
    }

    fn with_tally(&self, status: JobStatus, error: Option<&str>, tally: Tally) -> Finish {
        Finish {
            status,
            error: error.map(str::to_owned),
            prompt_tokens: tally.prompt,
            completion_tokens: tally.completion,
            pages: tally.pages,
            rejected: tally.rejected,
            facts: tally.facts,
            retry_at: None,
            chunks_done: None,
            refund_attempt: false,
        }
    }

    /// Creates the two pages every scope's folder starts from, when they
    /// are not there: the conventions (the person's to edit from then on)
    /// and the log.
    async fn seed(&self, scope: &str, now: OffsetDateTime) -> Result<(), PageError> {
        for (slug, title, body) in [
            (
                layout::SCHEMA,
                "Coding-agent wiki: conventions",
                prompt::DEFAULT_SCHEMA,
            ),
            (layout::LOG, "Coding-agent wiki: log", prompt::LOG_HEADER),
        ] {
            if self.pages.read(scope, slug).await?.is_some() {
                continue;
            }
            let place = layout::place(slug).expect("a collection slug");
            let markdown = frontmatter(&place, title, &utc_day(now), &[]) + body;
            match self
                .pages
                .write(
                    scope,
                    slug,
                    PageWrite {
                        entity_type: EntityType::Topic,
                        markdown,
                        author: Author::Brain { reconciles: None },
                        base_version: None,
                    },
                )
                .await
            {
                // A concurrent compile seeded it first: the same page.
                Ok(_) | Err(PageError::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    /// The pages one call is shown: the session's folder and its parents,
    /// any topic pages already in the folder, and — read-only — the other
    /// agents' pages for the same project, which the rollup merges.
    async fn context(&self, scope: &str, allowed: &Allowed) -> Result<Vec<Shown>, PageError> {
        let listed = self.pages.list(&[scope], 1_000).await?;
        let mut slugs = allowed.context_slugs();
        let prefix = allowed.folder_prefix();
        for summary in &listed {
            if slugs.len() >= 24 {
                break;
            }
            let extra = summary.slug.starts_with(&prefix)
                || matches!(
                    layout::place(&summary.slug),
                    Some(Place::Project { ref agent, ref project, ref page })
                        if *agent != allowed.agent
                            && *project == allowed.project
                            && (page == "decisions" || page == "index")
                );
            if extra && !slugs.contains(&summary.slug) {
                slugs.push(summary.slug.clone());
            }
        }
        let mut left = self.settings.context_chars;
        let mut shown = Vec::with_capacity(slugs.len());
        for slug in slugs {
            let writable = allowed.permits(&slug);
            match self.pages.read(scope, &slug).await? {
                Some(page) => {
                    let body = strip_frontmatter(&page.markdown);
                    let length = body.chars().count();
                    if length <= left {
                        left -= length;
                        shown.push(Shown {
                            slug,
                            version: Some(page.version),
                            body,
                            writable,
                        });
                    } else {
                        // Shown cut, and so not writable this call: a body
                        // the model only saw part of is a body it would
                        // truncate if it rewrote it.
                        let kept = record::cap(&body, left.max(400));
                        left = left.saturating_sub(kept.chars().count());
                        shown.push(Shown {
                            slug,
                            version: Some(page.version),
                            body: kept,
                            writable: false,
                        });
                    }
                }
                None => shown.push(Shown {
                    slug,
                    version: None,
                    body: String::new(),
                    writable,
                }),
            }
        }
        Ok(shown)
    }

    /// Writes one edit, if the session may write it and the version it was
    /// based on is still the head.
    async fn apply(
        &self,
        scope: &str,
        allowed: &Allowed,
        shown: &[Shown],
        edit: &PageEdit,
        head: &SessionHead,
        now: OffsetDateTime,
    ) -> Applied {
        // The namespace rule: the session's own folder and its parents,
        // never the log, never the conventions, never a page outside
        // `coding-agents/`.
        if !allowed.permits(&edit.slug) {
            return Applied::Rejected;
        }
        if shown
            .iter()
            .any(|page| page.slug == edit.slug && !page.writable)
        {
            return Applied::Rejected;
        }
        let Some(place) = layout::place(&edit.slug) else {
            return Applied::Rejected;
        };
        let current: Option<Page> = match self.pages.read(scope, &edit.slug).await {
            Ok(page) => page,
            Err(_) => return Applied::Conflict,
        };
        let base = current.as_ref().map(|page| page.version);
        if base != edit.base_version {
            return Applied::Conflict;
        }
        let Ok((body, _)) = redact(&strip_frontmatter(&edit.body), Policy::Redact) else {
            return Applied::Rejected;
        };
        let body = with_citation(&fix_links(&body), head);
        let title = edit
            .title
            .as_deref()
            .map(redacted_line)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| default_title(&place));
        let mut sources: Vec<String> = vec![head.source_id.clone()];
        if let Some(page) = &current
            && let Some(listed) = page.frontmatter.get("sources")
        {
            for id in listed.split(',').map(str::trim).filter(|id| !id.is_empty()) {
                if !sources.iter().any(|seen| seen == id) && sources.len() < 20 {
                    sources.push(id.to_owned());
                }
            }
        }
        let markdown = frontmatter(&place, &title, &utc_day(now), &sources) + &body;
        let mut reconciles = None;
        for _ in 0..2 {
            let write = PageWrite {
                entity_type: EntityType::Topic,
                markdown: markdown.clone(),
                author: Author::Brain { reconciles },
                base_version: base,
            };
            match self.pages.write(scope, &edit.slug, write).await {
                Ok(_) => return Applied::Written,
                // A person edited the page, and the head the model was
                // shown *is* that edit (the base is the head): the body
                // already incorporates it, so the brain may say so.
                Err(PageError::HumanEditPending { version })
                    if base.is_some_and(|base| version <= base) && reconciles.is_none() =>
                {
                    reconciles = Some(version);
                }
                Err(PageError::Conflict { .. } | PageError::HumanEditPending { .. }) => {
                    return Applied::Conflict;
                }
                Err(_) => return Applied::Rejected,
            }
        }
        Applied::Conflict
    }

    /// The facts as the layer will store them: the session's source cited,
    /// observed at compile time (when the brain learned it), valid from the
    /// session's day when the model gave no time, and the free text
    /// redacted. `me` becomes the syncing person's own subject.
    fn prepare_facts(
        &self,
        facts: &[Fact],
        job: &Job,
        head: &SessionHead,
        me: Option<&str>,
        now: OffsetDateTime,
    ) -> Vec<Fact> {
        let observed = now
            .to_offset(time::UtcOffset::UTC)
            .replace_nanosecond(0)
            .unwrap_or(now)
            .format(&time::format_description::well_known::Rfc3339)
            .ok();
        facts
            .iter()
            .map(|fact| Fact {
                subject: match (fact.subject.as_str(), me) {
                    ("me", Some(me)) => me.to_owned(),
                    _ => fact.subject.clone(),
                },
                source: Some(
                    fact.source
                        .clone()
                        .filter(|source| source.contains(&job.source_id))
                        .unwrap_or_else(|| format!("source:{}", job.source_id)),
                ),
                observed_at: observed.clone(),
                valid_from: fact
                    .valid_from
                    .clone()
                    .filter(|from| !from.trim().is_empty())
                    .or_else(|| Some(head.day.clone())),
                value: redacted_text(&fact.value),
                quote: fact.quote.as_deref().map(redacted_text),
                ..fact.clone()
            })
            .collect()
    }

    /// Appends one line to the log page — the one page only the server
    /// writes. Re-read and re-appended on a lost race, so a line is never
    /// dropped and never written twice.
    async fn append_log(&self, scope: &str, line: &str) -> bool {
        for _ in 0..4 {
            let Ok(Some(page)) = self.pages.read(scope, layout::LOG).await else {
                return false;
            };
            let mut markdown = page.markdown.clone();
            if !markdown.ends_with('\n') {
                markdown.push('\n');
            }
            markdown.push_str(line);
            markdown.push('\n');
            let mut reconciles = None;
            for _ in 0..2 {
                match self
                    .pages
                    .write(
                        scope,
                        layout::LOG,
                        PageWrite {
                            entity_type: EntityType::Topic,
                            markdown: markdown.clone(),
                            author: Author::Brain { reconciles },
                            base_version: Some(page.version),
                        },
                    )
                    .await
                {
                    Ok(_) => return true,
                    Err(PageError::HumanEditPending { version }) if version <= page.version => {
                        reconciles = Some(version);
                    }
                    Err(_) => break,
                }
            }
        }
        false
    }

    /// The model-free lint pass over one scope's collection: pages nothing
    /// links to, links to collection pages that do not exist, and pages no
    /// compile has touched in 60 days — written to `coding-agents--lint`
    /// when the report changed. Contradictions need a model and are left to
    /// the next version of this pass.
    ///
    /// # Errors
    ///
    /// [`PageError`] from the page store.
    pub async fn lint(&self, scope: &str) -> Result<bool, PageError> {
        let now = self.clock.now();
        let listed = self.pages.list(&[scope], 1_000).await?;
        let collection: Vec<_> = listed
            .iter()
            .filter(|page| layout::in_collection(&page.slug) && page.slug != layout::LINT)
            .take(200)
            .collect();
        if collection.is_empty() {
            return Ok(false);
        }
        let known: Vec<&str> = collection.iter().map(|page| page.slug.as_str()).collect();
        let mut orphans = Vec::new();
        let mut broken = Vec::new();
        let mut stale = Vec::new();
        let cutoff = crate::queue::rfc3339(now - Duration::days(60));
        for page in &collection {
            let top = matches!(layout::place(&page.slug), Some(Place::Top(_)));
            if !top && self.pages.backlinks(scope, &page.slug).await?.is_empty() {
                orphans.push(page.slug.clone());
            }
            if page.updated_at.as_str() < cutoff.as_str() {
                stale.push(page.slug.clone());
            }
            if let Some(read) = self.pages.read(scope, &page.slug).await? {
                for link in read.links {
                    if layout::in_collection(&link) && !known.contains(&link.as_str()) {
                        broken.push(format!("[[{}]] → `{link}`", page.slug));
                    }
                }
            }
        }
        let section = |title: &str, items: &[String], empty: &str| {
            let mut out = format!("## {title}\n\n");
            if items.is_empty() {
                out.push_str(empty);
                out.push('\n');
            }
            for item in items {
                if item.starts_with("[[") {
                    out.push_str(&format!("- {item}\n"));
                } else {
                    out.push_str(&format!("- [[{item}]]\n"));
                }
            }
            out.push('\n');
            out
        };
        let body = format!(
            "# Coding-agent wiki: lint\n\nA model-free check of the folder, rewritten when it changes. Contradictions are not checked yet.\n\n{}{}{}",
            section("Orphans (nothing links here)", &orphans, "None."),
            section("Broken links", &broken, "None."),
            section("Stale (not compiled into for 60 days)", &stale, "None."),
        );
        let place = Place::Top("lint".to_owned());
        let current = self.pages.read(scope, layout::LINT).await?;
        if current
            .as_ref()
            .is_some_and(|page| strip_frontmatter(&page.markdown).trim() == body.trim())
        {
            return Ok(false);
        }
        let markdown = frontmatter(&place, "Coding-agent wiki: lint", &utc_day(now), &[]) + &body;
        let base = current.as_ref().map(|page| page.version);
        match self
            .pages
            .write(
                scope,
                layout::LINT,
                PageWrite {
                    entity_type: EntityType::Topic,
                    markdown,
                    author: Author::Brain { reconciles: None },
                    base_version: base,
                },
            )
            .await
        {
            Ok(_) => Ok(true),
            // A person edited the report, or another pass won: leave it.
            Err(PageError::HumanEditPending { .. } | PageError::Conflict { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn parked(job: &Job, status: JobStatus, reason: &str) -> Processed {
    Processed {
        source_id: job.source_id.clone(),
        status,
        pages: Vec::new(),
        error: Some(reason.to_owned()),
    }
}

/// The last path segment of a recorded repository path.
fn basename(repo: &str) -> Option<&str> {
    repo.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty() && *name != "~")
}

/// The body after a leading `---` frontmatter fence, or the text itself.
#[must_use]
pub fn strip_frontmatter(markdown: &str) -> String {
    let trimmed = markdown.trim_start_matches('\u{feff}');
    let mut lines = trimmed.split_inclusive('\n');
    let Some(first) = lines.next() else {
        return String::new();
    };
    if first.trim_end() != "---" {
        return trimmed.to_owned();
    }
    let mut offset = first.len();
    for line in lines {
        offset += line.len();
        if line.trim_end() == "---" {
            return trimmed[offset..].trim_start_matches('\n').to_owned();
        }
    }
    trimmed.to_owned()
}

/// A compiled page's frontmatter: the Topic schema's keys, the folder it
/// sits in, the day it was last compiled and the sources behind it.
fn frontmatter(place: &Place, title: &str, day: &str, sources: &[String]) -> String {
    let mut out = String::from("---\ntype: topic\n");
    out.push_str(&format!("title: {}\n", edits::one_line(title, 120)));
    out.push_str(&format!("collection: {}\n", layout::PREFIX));
    match place {
        Place::Agent { agent, .. } => out.push_str(&format!("agent: {agent}\n")),
        Place::Project { agent, project, .. } => {
            out.push_str(&format!("agent: {agent}\nproject: {project}\n"));
        }
        Place::Rollup { project } => out.push_str(&format!("project: {project}\n")),
        Place::Top(_) => {}
    }
    out.push_str(&format!("page: {}\n", layout::page_kind(place)));
    out.push_str(&format!("updated: {day}\n"));
    if !sources.is_empty() {
        out.push_str(&format!("sources: {}\n", sources.join(", ")));
    }
    out.push_str("---\n\n");
    out
}

/// The title a page gets when the model named none.
fn default_title(place: &Place) -> String {
    match place {
        Place::Top(page) => format!("Coding agents: {page}"),
        Place::Agent { agent, page } => format!("{agent}: {page}"),
        Place::Rollup { project } => format!("Project {project}: all agents"),
        Place::Project {
            agent,
            project,
            page,
        } => format!("{agent} / {project}: {page}"),
    }
}

/// `[[target]]` links the page store would refuse (not a slug) become
/// plain text, so one bad link cannot cost a whole page.
fn fix_links(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("]]") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        let (target, label) = inner.split_once('|').unwrap_or((inner, inner));
        let slug = target.trim().to_ascii_lowercase();
        if livingbrain_pages::is_slug(&slug) {
            out.push_str("[[");
            out.push_str(inner);
            out.push_str("]]");
        } else {
            out.push_str(label.trim());
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Makes sure the page cites this session: a `## Sources` list at the end
/// gains the session's link when the body does not already name it.
fn with_citation(body: &str, head: &SessionHead) -> String {
    let mut out = body.trim_end().to_owned();
    if out.contains(&head.source_id) {
        out.push('\n');
        return out;
    }
    if !out.contains("\n## Sources") && !out.starts_with("## Sources") {
        out.push_str("\n\n## Sources\n");
    }
    out.push_str(&format!(
        "\n- [session {} · {}]({})\n",
        head.day,
        head.agent_folder,
        prompt::source_href(&head.source_id)
    ));
    out
}

/// One redacted line of model text.
fn redacted_line(text: &str) -> String {
    edits::one_line(&redacted_text(text), 300)
}

/// Model text redacted; a text redaction refuses becomes nothing.
fn redacted_text(text: &str) -> String {
    redact(text, Policy::Redact)
        .map(|(clean, _)| clean)
        .unwrap_or_default()
}

/// The most fact refusals one log line spells out; the rest are counted.
const MAX_SKIPS_LOGGED: usize = 5;

/// The log line for one compiled session.
fn log_line(head: &SessionHead, tally: &Tally, omitted: bool, now: OffsetDateTime) -> String {
    let short: String = head.session_id.chars().take(8).collect();
    let summary = if tally.summaries.is_empty() {
        "nothing new".to_owned()
    } else {
        tally.summaries.join(" ")
    };
    let pages = tally
        .pages
        .iter()
        .map(|slug| format!("[[{slug}]]"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = format!(
        "- **{}** · {}/{} · session `{short}` of {} · {}",
        utc_day(now),
        head.agent_folder,
        head.project,
        head.day,
        edits::one_line(&summary, 300)
    );
    if !pages.is_empty() {
        line.push_str(&format!(" · pages: {pages}"));
    }
    if omitted {
        line.push_str(" · transcript cut to budget");
    }
    if tally.facts > 0 {
        line.push_str(&format!(" · facts recorded: {}", tally.facts));
    }
    if !tally.skipped_facts.is_empty() {
        let shown = tally
            .skipped_facts
            .iter()
            .take(MAX_SKIPS_LOGGED)
            .map(|reason| edits::one_line(&redacted_text(reason), 160))
            .collect::<Vec<_>>()
            .join("; ");
        let more = tally.skipped_facts.len().saturating_sub(MAX_SKIPS_LOGGED);
        line.push_str(&format!(
            " · facts skipped: {} ({shown}{})",
            tally.skipped_facts.len(),
            if more > 0 {
                format!("; {more} more")
            } else {
                String::new()
            }
        ));
    }
    if let Some((count, reason)) = &tally.unrecorded_facts {
        line.push_str(&format!(
            " · facts not recorded: {count} ({})",
            edits::one_line(&redacted_text(reason), 160)
        ));
    }
    line.push_str(&format!(
        " · [source]({})",
        prompt::source_href(&head.source_id)
    ));
    line
}

fn start_of_next_day(now: OffsetDateTime) -> OffsetDateTime {
    let date = now.to_offset(time::UtcOffset::UTC).date();
    date.next_day().unwrap_or(date).midnight().assume_utc()
}

fn start_of_next_month(now: OffsetDateTime) -> OffsetDateTime {
    let date = now.to_offset(time::UtcOffset::UTC).date();
    let (year, month) = if date.month() == time::Month::December {
        (date.year() + 1, time::Month::January)
    } else {
        (date.year(), date.month().next())
    };
    time::Date::from_calendar_date(year, month, 1)
        .unwrap_or(date)
        .midnight()
        .assume_utc()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_is_stripped_once() {
        assert_eq!(strip_frontmatter("---\ntitle: x\n---\n\nbody"), "body");
        assert_eq!(strip_frontmatter("no fence"), "no fence");
        assert_eq!(strip_frontmatter("---\nunclosed"), "---\nunclosed");
    }

    #[test]
    fn a_bad_link_becomes_text() {
        assert_eq!(
            fix_links("see [[coding-agents--index]] and [[Some Page|that]] and [[x y]]"),
            "see [[coding-agents--index]] and that and x y"
        );
    }

    #[test]
    fn the_month_rolls_over() {
        let day = |y, m, d| {
            time::Date::from_calendar_date(y, m, d)
                .expect("a date")
                .midnight()
                .assume_utc()
        };
        let at = day(2026, time::Month::December, 31) + Duration::hours(23);
        assert_eq!(start_of_next_month(at), day(2027, time::Month::January, 1));
        assert_eq!(start_of_next_day(at), day(2027, time::Month::January, 1));
    }
}
