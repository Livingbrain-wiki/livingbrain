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

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cratefield_core::{Classifier, Clock, Config, DbError, ScheduledBudget};
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use livingbrain_pages::{
    Author, AuthorKind, EntityType, Page, PageError, PageStore, PageWrite, SourceError, SourceStore,
};
use livingbrain_redact::{Policy, redact};
use time::{Duration, OffsetDateTime};

use crate::compact::{self, CallRef, Keep};
use crate::edits::{self, PageEdit};
use crate::facts::{Fact, FactSink};
use crate::layout::{self, Allowed, Place};
use crate::merge;
use crate::pool::{CONCURRENCY_CEILING, MAX_IN_FLIGHT, Tick};
use crate::prompt::{self, SessionHead, Shown};
use crate::queue::{ClaimFilter, Finish, Job, JobStatus, MAX_ATTEMPTS, Queue, utc_day};
use crate::record::{self, Session};
use crate::triage::{self, Decision, Plan, Strictness, Triage};

/// The most sessions one cron run may compile for one workspace, whatever
/// the workspace or the deployment asks for. A cron run's compiles are
/// bounded by time and connections, not steps: on Workers Paid the
/// venture's scheduled budget gives the compiler a share of 2,000 steps
/// (about 660, eight a compile — 80 compiles' worth), while at most
/// [`MAX_IN_FLIGHT`] compiles run at once (a Worker invocation keeps six
/// connections open) and they start only until the share's deadline (about
/// four minutes in, at most five and a half). Twenty is past what a run
/// gets through, so the clamp only keeps a stored or configured number from
/// asking for more than one invocation can finish.
pub const BATCH_CEILING: usize = 20;
/// The most model calls one session may take, a hard ceiling for the same
/// reason as [`BATCH_CEILING`]: every call is a subrequest and a wait.
pub const MAX_CHUNKS_CEILING: usize = 20;
/// The hard cap on one session's tokens, in and out: one compile runs inside
/// a single cron invocation's wall (minutes) or a single "Process now"
/// request, so a per-session cap past this would be a compile that times out
/// half-written rather than one that finishes.
pub const JOB_TOKENS_CEILING: u64 = 1_000_000;
/// The largest transcript chunk one call may send, in characters (~50k
/// tokens): past it a call outgrows the windows most connected models have.
pub const CHUNK_CHARS_CEILING: usize = 200_000;
/// The largest completion one compile call may ask for.
pub const MAX_OUTPUT_TOKENS_CEILING: u32 = 32_000;

/// A workspace's own choices over the compiler's knobs (Settings → Usage &
/// limits). `None` keeps the deployment's value; whatever is set is clamped
/// to the same floors and hard ceilings the env vars are.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub paused: Option<bool>,
    pub batch: Option<u64>,
    pub concurrency: Option<u64>,
    pub chunk_chars: Option<u64>,
    pub max_chunks: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub job_tokens: Option<u64>,
    pub daily_tokens: Option<u64>,
    pub monthly_tokens: Option<u64>,
    pub triage: Option<bool>,
    pub compaction: Option<bool>,
    pub triage_strictness: Option<Strictness>,
}

/// Where a workspace's [`Overrides`] come from. The composition wires one
/// over the workspace settings table; a compiler without one runs every
/// workspace on the deployment's settings.
#[async_trait::async_trait]
pub trait Tuning: Send + Sync {
    /// The workspace's overrides. A read that fails is no overrides.
    async fn overrides(&self, workspace: &str) -> Overrides;
}

/// No overrides: every workspace runs on the deployment's settings.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTuning;

#[async_trait::async_trait]
impl Tuning for NoTuning {
    async fn overrides(&self, _workspace: &str) -> Overrides {
        Overrides::default()
    }
}

/// Which triage classifier (Jev) a workspace's compiles ask, if any. The
/// composition wires one over the workspace's own Jev settings (its switch,
/// its own TypeSafe key, else the deployment's); a compiler without one
/// asks no classifier and the free rule triages alone.
#[async_trait::async_trait]
pub trait Classifiers: Send + Sync {
    /// The workspace's classifier, or `None` (switched off, no key, or a
    /// read that failed).
    async fn for_workspace(&self, workspace: &str) -> Option<Arc<dyn Classifier>>;
}

/// One classifier for every workspace: what [`Compiler::with_classifier`]
/// wires.
struct OneClassifier(Arc<dyn Classifier>);

#[async_trait::async_trait]
impl Classifiers for OneClassifier {
    async fn for_workspace(&self, _workspace: &str) -> Option<Arc<dyn Classifier>> {
        Some(self.0.clone())
    }
}

/// The knobs, all from config (`CODING_AGENTS_*`), all with defaults that
/// keep one session's compile small and a workspace's month bounded. A
/// workspace's own [`Overrides`] apply on top ([`Settings::with_overrides`]).
#[derive(Debug, Clone)]
pub struct Settings {
    /// Nothing compiles while set (a workspace's own pause; there is no env
    /// var for it). Default off.
    pub paused: bool,
    /// Jobs one cron tick compiles (`CODING_AGENTS_BATCH`, default 1).
    pub batch: usize,
    /// Jobs of the workspace one drain compiles at once
    /// (`CODING_AGENTS_CONCURRENCY`, default 4, never more than
    /// [`CONCURRENCY_CEILING`]; the invocation runs at most
    /// [`crate::MAX_IN_FLIGHT`] across workspaces). A 429 halves it for a
    /// while ([`crate::pool`]).
    pub concurrency: usize,
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
    /// Triage before a compile (`CODING_AGENTS_TRIAGE`, default on):
    /// trivial sessions skipped, failing loops and lookups summarised in
    /// one model-free log line ([`crate::triage`]).
    pub triage: bool,
    /// Compaction of the transcript before a compile
    /// (`CODING_AGENTS_COMPACTION`, default on; [`crate::compact`]).
    pub compaction: bool,
    /// How readily triage holds a session back
    /// (`CODING_AGENTS_TRIAGE_STRICTNESS`, default `balanced`).
    pub strictness: Strictness,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            paused: false,
            batch: 1,
            concurrency: 4,
            chunk_chars: 32_000,
            max_chunks: 4,
            context_chars: 32_000,
            max_output_tokens: 6_000,
            job_tokens: 80_000,
            daily_tokens: 400_000,
            monthly_tokens: 4_000_000,
            lease: Duration::minutes(15),
            triage: true,
            compaction: true,
            strictness: Strictness::Balanced,
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
        let switch = |key: &str, fallback: bool| {
            config
                .get(key)
                .and_then(|raw| match raw.trim().to_ascii_lowercase().as_str() {
                    "1" | "true" | "on" | "yes" => Some(true),
                    "0" | "false" | "off" | "no" => Some(false),
                    _ => None,
                })
                .unwrap_or(fallback)
        };
        Self {
            paused: false,
            batch: size("CODING_AGENTS_BATCH", default.batch),
            concurrency: size("CODING_AGENTS_CONCURRENCY", default.concurrency),
            chunk_chars: size("CODING_AGENTS_CHUNK_CHARS", default.chunk_chars),
            max_chunks: size("CODING_AGENTS_MAX_CHUNKS", default.max_chunks),
            context_chars: size("CODING_AGENTS_CONTEXT_CHARS", default.context_chars).max(1_000),
            max_output_tokens: u32::try_from(number(
                "CODING_AGENTS_MAX_OUTPUT_TOKENS",
                u64::from(default.max_output_tokens),
            ))
            .unwrap_or(default.max_output_tokens),
            job_tokens: number("CODING_AGENTS_JOB_TOKENS", default.job_tokens),
            daily_tokens: number("CODING_AGENTS_DAILY_TOKENS", default.daily_tokens),
            monthly_tokens: number("CODING_AGENTS_MONTHLY_TOKENS", default.monthly_tokens),
            lease: default.lease,
            triage: switch("CODING_AGENTS_TRIAGE", default.triage),
            compaction: switch("CODING_AGENTS_COMPACTION", default.compaction),
            strictness: config
                .get("CODING_AGENTS_TRIAGE_STRICTNESS")
                .and_then(|raw| Strictness::parse(&raw))
                .unwrap_or(default.strictness),
        }
        .bounded()
    }

    /// These settings with a workspace's own choices on top: each one that
    /// is set replaces the deployment's value, and the result is clamped to
    /// the same floors and hard ceilings ([`BATCH_CEILING`],
    /// [`JOB_TOKENS_CEILING`], …), so no stored number can lift a compile
    /// past what one Worker invocation can finish.
    #[must_use]
    pub fn with_overrides(&self, overrides: &Overrides) -> Self {
        let size = |value: Option<u64>, fallback: usize| {
            value.map_or(fallback, |value| {
                usize::try_from(value).unwrap_or(usize::MAX)
            })
        };
        Self {
            paused: overrides.paused.unwrap_or(self.paused),
            batch: size(overrides.batch, self.batch),
            concurrency: size(overrides.concurrency, self.concurrency),
            chunk_chars: size(overrides.chunk_chars, self.chunk_chars),
            max_chunks: size(overrides.max_chunks, self.max_chunks),
            context_chars: self.context_chars,
            max_output_tokens: overrides
                .max_output_tokens
                .map_or(self.max_output_tokens, |value| {
                    u32::try_from(value).unwrap_or(MAX_OUTPUT_TOKENS_CEILING)
                }),
            job_tokens: overrides.job_tokens.unwrap_or(self.job_tokens),
            daily_tokens: overrides.daily_tokens.unwrap_or(self.daily_tokens),
            monthly_tokens: overrides.monthly_tokens.unwrap_or(self.monthly_tokens),
            lease: self.lease,
            triage: overrides.triage.unwrap_or(self.triage),
            compaction: overrides.compaction.unwrap_or(self.compaction),
            strictness: overrides.triage_strictness.unwrap_or(self.strictness),
        }
        .bounded()
    }

    /// The floors and the hard ceilings, applied to every source alike.
    fn bounded(mut self) -> Self {
        self.batch = self.batch.clamp(1, BATCH_CEILING);
        self.concurrency = self.concurrency.clamp(1, CONCURRENCY_CEILING);
        self.chunk_chars = self.chunk_chars.clamp(1_000, CHUNK_CHARS_CEILING);
        self.max_chunks = self.max_chunks.clamp(1, MAX_CHUNKS_CEILING);
        self.max_output_tokens = self.max_output_tokens.clamp(256, MAX_OUTPUT_TOKENS_CEILING);
        self.job_tokens = self.job_tokens.min(JOB_TOKENS_CEILING);
        self
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
    /// The provider stopped at the completion ceiling (`max_tokens`): the
    /// text is likely cut off mid-answer.
    pub truncated: bool,
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
    /// The call failed. The string starts with a short fixed-vocabulary
    /// reason (`http`, `bad-response`, …) and may go on with what the
    /// provider said, already made safe to show (`http 400: {…}`).
    Failed(String),
    /// The provider is rate-limiting the key (HTTP 429): nothing was spent,
    /// and the same call later is expected to work. The job is retried
    /// after `retry_after_secs` (or a few minutes) without counting an
    /// attempt, and the drain stops — the next job would be refused too.
    Busy {
        reason: String,
        retry_after_secs: Option<u64>,
    },
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
    tuning: Arc<dyn Tuning>,
    /// The triage classifiers (Jev, per workspace), when the venture wires
    /// them.
    classifiers: Option<Arc<dyn Classifiers>>,
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
            tuning: Arc::new(NoTuning),
            classifiers: None,
        }
    }

    /// The same compiler, asking `classifier` (Jev) once per session before
    /// a compile: how much of a compile it deserves, which topic pages it
    /// bears on, which bulky outputs it needs word for word
    /// ([`crate::triage::Plan::ask`]). Without one, the free rule decides.
    #[must_use]
    pub fn with_classifier(mut self, classifier: Option<Arc<dyn Classifier>>) -> Self {
        self.classifiers =
            classifier.map(|one| Arc::new(OneClassifier(one)) as Arc<dyn Classifiers>);
        self
    }

    /// The same compiler, asking each workspace's own classifier
    /// ([`Classifiers::for_workspace`]) — the workspace's switch and key
    /// decide, per compile.
    #[must_use]
    pub fn with_classifiers(mut self, classifiers: Option<Arc<dyn Classifiers>>) -> Self {
        self.classifiers = classifiers;
        self
    }

    /// The same compiler, reading each workspace's own [`Overrides`] from
    /// `tuning` before it compiles anything of theirs.
    #[must_use]
    pub fn with_tuning(mut self, tuning: Arc<dyn Tuning>) -> Self {
        self.tuning = tuning;
        self
    }

    /// The settings `workspace` compiles under: the deployment's, with the
    /// workspace's own overrides on top.
    pub async fn settings_for(&self, workspace: &str) -> Settings {
        self.settings
            .with_overrides(&self.tuning.overrides(workspace).await)
    }

    /// The queue this compiler drains.
    #[must_use]
    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Enqueues what the ledger holds, then compiles up to `max_jobs` jobs
    /// the filter allows, oldest first — parking the ones whose workspace
    /// has no model, no budget left or a paused compiler without spending
    /// anything on them. Each job compiles under its workspace's own
    /// settings ([`Compiler::settings_for`]). A drain across every workspace
    /// (no `filter.workspace`: the cron) also takes at most each
    /// workspace's own `batch` of jobs. `budget` is the scheduled
    /// invocation's cooperative budget (unbounded outside a cron tick); the
    /// drain starts nothing once it runs out.
    ///
    /// **In parallel.** Up to each workspace's `concurrency` compiles run at
    /// once, never more than [`MAX_IN_FLIGHT`] in all, the next one started
    /// as soon as one ends — taken from the workspace that has started the
    /// fewest, so one workspace's backlog does not starve another's. Within
    /// a compile the chunks stay in order (each builds on the pages the last
    /// one wrote). A 429 pauses that workspace's pool; a cron drain starts a
    /// compile only while its deadline leaves room for a call of average
    /// length ([`crate::pool`]).
    ///
    /// # Errors
    ///
    /// [`DbError`] when the queue itself cannot be read or written. A
    /// failing job is a `failed` row, not an error here; the compiles
    /// already running when an error comes are finished first.
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
        self.queue.clear_lapsed_holds(now).await?;
        let per_workspace = max_jobs.saturating_mul(2).max(4);
        let ready = self
            .queue
            .ready_fair(
                now,
                filter,
                per_workspace.saturating_mul(16),
                filter.workspace.is_none().then_some(per_workspace),
            )
            .await?;
        let tick = Tick::new(budget, filter.ignore_backoff);
        let mut lanes: Vec<Lane> = Vec::new();
        for job in ready {
            match lanes
                .iter_mut()
                .find(|lane| lane.workspace == job.workspace)
            {
                Some(lane) => lane.jobs.push_back(job),
                None => lanes.push(Lane::new(job)),
            }
        }
        let mut models: BTreeMap<(String, String), bool> = BTreeMap::new();
        let mut out = Vec::new();
        let mut started = 0usize;
        let mut stop = false;
        let mut failure: Option<DbError> = None;
        let mut running: FuturesUnordered<Running<'_>> = FuturesUnordered::new();
        loop {
            while !stop && failure.is_none() && running.len() < MAX_IN_FLIGHT && started < max_jobs
            {
                let Some(index) = next_lane(&lanes, &tick, filter) else {
                    break;
                };
                let lane = &mut lanes[index];
                if lane.settings.is_none() {
                    let settings = self.settings_for(&lane.workspace).await;
                    // A pool that cannot be read starts from the setting.
                    let pool = self.queue.pool(&lane.workspace).await.unwrap_or_default();
                    tick.open(&lane.workspace, &pool, settings.concurrency);
                    lane.settings = Some(settings);
                }
                let settings = lane.settings.clone().unwrap_or_default();
                let Some(job) = lane.jobs.pop_front() else {
                    continue;
                };
                let admitted = match self.admit(job, &settings, &mut models, &tick, budget).await {
                    Ok(admitted) => admitted,
                    // Start nothing more; what runs is finished first.
                    Err(error) => {
                        failure.get_or_insert(error);
                        continue;
                    }
                };
                match admitted {
                    Admit::Parked(processed) => out.push(processed),
                    Admit::Gone => {}
                    Admit::Later => lane.closed = true,
                    Admit::Stop => stop = true,
                    Admit::Run(claimed) => {
                        lane.started += 1;
                        lane.running += 1;
                        started += 1;
                        let tick = &tick;
                        let claimed = *claimed;
                        running.push(Box::pin(async move {
                            let finish = self.process(&claimed, &settings, tick).await;
                            let recorded =
                                self.queue.finish(&claimed, &finish, self.clock.now()).await;
                            (index, claimed, finish, recorded)
                        }));
                    }
                }
            }
            let Some((index, claimed, finish, recorded)) = running.next().await else {
                break;
            };
            let lane = &mut lanes[index];
            lane.running = lane.running.saturating_sub(1);
            if let Err(error) = recorded {
                // Finish what is running, start nothing more, then say so.
                failure.get_or_insert(error);
                continue;
            }
            if finish.status == JobStatus::Done {
                lane.done += 1;
            }
            if finish.status == JobStatus::Failed {
                // Workers Logs: the one place a cron tick's failure shows
                // without opening the queue. The reason is fixed-vocabulary
                // plus a provider body already made safe to show.
                eprintln!(
                    "coding-agents: job for source {} failed (attempt {}): {}",
                    claimed.source_id,
                    claimed.attempts,
                    finish.error.as_deref().unwrap_or("unknown")
                );
            }
            out.push(Processed {
                source_id: claimed.source_id.clone(),
                status: finish.status,
                pages: finish.pages.clone(),
                error: finish.error.clone(),
            });
        }
        // What the next drain finds: each pool, paused or grown, and the
        // average call it timed.
        let end = self.clock.now();
        for lane in lanes.iter().filter(|lane| lane.settings.is_some()) {
            if let Some(row) = tick.row(&lane.workspace, lane.done) {
                self.queue.save_pool(&lane.workspace, &row, end).await?;
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(out),
        }
    }

    /// Whether one job read by the drain runs now: parked (no model, no
    /// budget, paused), left for later (its pool is waiting or full, or the
    /// tick has no time for it), the end of the drain (no subrequests left),
    /// or claimed.
    async fn admit(
        &self,
        job: Job,
        settings: &Settings,
        models: &mut BTreeMap<(String, String), bool>,
        tick: &Tick,
        budget: &ScheduledBudget,
    ) -> Result<Admit, DbError> {
        let now = self.clock.now();
        if budget.exhausted(now) {
            return Ok(Admit::Stop);
        }
        if settings.paused {
            // Parked, not skipped: a skipped job would be read again on
            // every tick and crowd out the workspaces that are running.
            let parked = self
                .queue
                .park(
                    &job,
                    JobStatus::OverBudget,
                    now,
                    now + Duration::minutes(15),
                    "paused",
                )
                .await?;
            return Ok(if parked {
                Admit::Parked(self::parked(&job, JobStatus::OverBudget, "paused"))
            } else {
                Admit::Gone
            });
        }
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
            let parked = self
                .queue
                .park(
                    &job,
                    JobStatus::SkippedNoModel,
                    now,
                    now + Duration::hours(1),
                    "no-model",
                )
                .await?;
            return Ok(if parked {
                Admit::Parked(self::parked(&job, JobStatus::SkippedNoModel, "no-model"))
            } else {
                Admit::Gone
            });
        }
        match self.over_budget(&job.workspace, now, settings).await? {
            Budget::Spent(until) => {
                let parked = self
                    .queue
                    .park(&job, JobStatus::OverBudget, now, until, "over-budget")
                    .await?;
                return Ok(if parked {
                    Admit::Parked(self::parked(&job, JobStatus::OverBudget, "over-budget"))
                } else {
                    Admit::Gone
                });
            }
            // What is left is held by calls in flight: it may come back
            // when they settle, so the job stays queued.
            Budget::Held => return Ok(Admit::Later),
            Budget::Open => {}
        }
        // A paused pool (a 429) or a tick without time for another call
        // leaves the job queued, untouched.
        if !tick.may_start(&job.workspace, now) {
            return Ok(Admit::Later);
        }
        // A cron tick's share is counted in subrequest-shaped steps; a
        // compile is several (the source, the pages, the model calls).
        if !budget.try_spend(if self.classifiers.is_some() { 9 } else { 8 }) {
            return Ok(Admit::Stop);
        }
        if !self.queue.claim(&job, now, self.settings.lease).await? {
            return Ok(Admit::Gone);
        }
        Ok(Admit::Run(Box::new(Job {
            attempts: job.attempts + 1,
            status: JobStatus::Processing,
            ..job
        })))
    }

    /// Whether the workspace may compile now: `Spent` (until when it may
    /// reset) when its daily or monthly compile tokens are spent or the
    /// usage gate says it is over its budget, `Held` when what is left is
    /// held by calls in flight, else `Open`.
    async fn over_budget(
        &self,
        workspace: &str,
        now: OffsetDateTime,
        settings: &Settings,
    ) -> Result<Budget, DbError> {
        let today = utc_day(now);
        let month_start = format!("{}-01", &today[..7]);
        let spent_today = self.queue.spent(workspace, &today, &today).await?;
        let spent_month = self.queue.spent(workspace, &month_start, &today).await?;
        if spent_month >= settings.monthly_tokens {
            return Ok(Budget::Spent(start_of_next_month(now)));
        }
        if spent_today >= settings.daily_tokens {
            return Ok(Budget::Spent(start_of_next_day(now)));
        }
        if self.gate.over_budget(workspace).await {
            return Ok(Budget::Spent(now + Duration::hours(6)));
        }
        let held_today = self.queue.held(workspace, &today, &today, now).await?;
        let held_month = self
            .queue
            .held(workspace, &month_start, &today, now)
            .await?;
        if spent_month.saturating_add(held_month) >= settings.monthly_tokens
            || spent_today.saturating_add(held_today) >= settings.daily_tokens
        {
            return Ok(Budget::Held);
        }
        Ok(Budget::Open)
    }

    /// Compiles one claimed job, and says how it ended. Never fails: every
    /// failure is a status the queue records.
    async fn process(&self, job: &Job, settings: &Settings, tick: &Tick) -> Finish {
        let mut stamp = Stamp::default();
        let finish = self.process_job(job, settings, &mut stamp, tick).await;
        Finish {
            triage: stamp.triage,
            estimate: stamp.estimate,
            ..finish
        }
    }

    /// [`Compiler::process`]'s body. What triage decided and what the
    /// compile is estimated to send go on `stamp` as soon as they are known,
    /// so every way out of here records them.
    async fn process_job(
        &self,
        job: &Job,
        settings: &Settings,
        stamp: &mut Stamp,
        tick: &Tick,
    ) -> Finish {
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
        let (raw_chunks, _) = session.chunks(settings.chunk_chars, settings.max_chunks);
        if raw_chunks.is_empty() || session.turns.is_empty() {
            return Finish::status(JobStatus::Skipped, Some("empty-session"));
        }

        // Triage, then compaction: what the session deserves, and what of
        // it the model is sent. The source itself is never changed.
        let signals = triage::signals(&session);
        let plan = self.plan(job, &session, &signals, &allowed, settings).await;
        // Every call carries the fixed prompt and the conventions; the
        // pages shown vary per call and are left out, so the estimate is
        // a floor on both sides.
        let overhead =
            prompt::estimate_tokens(prompt::SYSTEM) + prompt::estimate_tokens(&conventions);
        let cost = |chunks: &[String]| -> u64 {
            chunks
                .iter()
                .map(|chunk| prompt::estimate_tokens(chunk) + overhead)
                .sum()
        };
        let raw_tokens = cost(&raw_chunks);
        stamp.triage = Some((
            plan.decision.triage.as_str().to_owned(),
            plan.decision.reason.clone(),
            plan.stored(),
        ));
        stamp.estimate = Some((raw_tokens, 0));
        match plan.decision.triage {
            Triage::Skip => {
                return Finish::status(JobStatus::SkippedTrivial, Some(&plan.decision.reason));
            }
            Triage::Summary => {
                let line = summary_line(&head, &signals, &plan.decision.reason, now);
                let mut finish =
                    Finish::status(JobStatus::SummaryOnly, Some(&plan.decision.reason));
                if self.append_log(&job.scope, &line, tick).await {
                    finish.pages.push(layout::LOG.to_owned());
                }
                return finish;
            }
            Triage::Full => {}
        }
        let sent_session = if plan.compact {
            compact::compact(&session, &plan.keep).0
        } else {
            session.clone()
        };
        let (chunks, mut omitted) = sent_session.chunks(settings.chunk_chars, settings.max_chunks);
        stamp.estimate = Some((raw_tokens.max(cost(&chunks)), cost(&chunks)));

        let mut tally = Tally::default();
        let parts = chunks.len();
        // A compile that waited on the owner's runner resumes after the
        // chunks it already applied; their tokens are on the row already.
        let resumed = usize::try_from(job.chunks_done).unwrap_or(0).min(parts);
        let spent_before = job.prompt_tokens.saturating_add(job.completion_tokens);
        'chunks: for (index, chunk) in chunks.iter().enumerate().skip(resumed) {
            let shown = match self
                .context(&job.scope, &allowed, plan.topics.as_deref())
                .await
            {
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
            // One call, and when its answer cannot be used, at most one
            // repair round: the same request with what was wrong appended
            // (and, for an answer cut off at the output limit, a larger
            // limit within the job's budget). Both count toward it.
            let mut ask = user.clone();
            let mut max_tokens = settings.max_output_tokens;
            let mut round = 0u32;
            let mut unusable: Option<(Unusable, bool, u32)> = None;
            let output = loop {
                let estimate = prompt::estimate_tokens(prompt::SYSTEM)
                    + prompt::estimate_tokens(&ask)
                    + u64::from(max_tokens);
                if spent_before
                    .saturating_add(tally.tokens())
                    .saturating_add(estimate)
                    > settings.job_tokens
                {
                    if round == 0 {
                        omitted = true;
                        break 'chunks;
                    }
                    // No budget left for the repair: the first answer's fault
                    // stands (`unusable` below was set by the round before).
                    break None;
                }
                // Before each call: the pool is not waiting out a 429 and the
                // invocation has time for it — else the job stops here and is
                // queued again, carrying on from this chunk.
                if let Some(halt) = tick.halt(&job.workspace, self.clock.now()) {
                    return Finish {
                        chunks_done: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                        refund_attempt: true,
                        ..self.with_tally(JobStatus::Queued, Some(halt.reason()), tally)
                    };
                }
                let ref_key = match round {
                    0 => format!("{}:{}:{index}", job.scope, job.body_sha256),
                    _ => format!("{}:{}:{index}:repair", job.scope, job.body_sha256),
                };
                // The call's share of the budget is held before it is made, so
                // parallel compiles cannot together spend past the budget.
                let hold = format!("{ref_key}:{}", job.attempts);
                let at = self.clock.now();
                match self
                    .queue
                    .reserve(
                        &hold,
                        &job.workspace,
                        at,
                        estimate,
                        settings.daily_tokens,
                        settings.monthly_tokens,
                        at + settings.lease,
                    )
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => {
                        let chunks_done = Some(u32::try_from(index).unwrap_or(u32::MAX));
                        return match self.over_budget(&job.workspace, at, settings).await {
                            Ok(Budget::Spent(until)) => Finish {
                                retry_at: Some(until),
                                chunks_done,
                                refund_attempt: true,
                                ..self.with_tally(JobStatus::OverBudget, Some("over-budget"), tally)
                            },
                            // Held by the calls in flight: queued again, to carry
                            // on when they settled.
                            Ok(_) => Finish {
                                chunks_done,
                                refund_attempt: true,
                                ..self.with_tally(JobStatus::Queued, Some("budget-held"), tally)
                            },
                            Err(_) => self.failed(job, now, "store", tally),
                        };
                    }
                    Err(_) => return self.failed(job, now, "store", tally),
                }
                let call = Call {
                    workspace: &job.workspace,
                    owner: &job.owner,
                    ref_key: &ref_key,
                };
                let call_started = self.clock.now();
                let answered = self
                    .model
                    .complete(&call, prompt::SYSTEM, &ask, max_tokens)
                    .await;
                let at = self.clock.now();
                // A runner's tokens were the owner's subscription, not the
                // workspace's compile budget; a call that did not answer spent
                // nothing.
                let spent = answered
                    .as_ref()
                    .ok()
                    .filter(|reply| !reply.runner)
                    .map(|reply| reply.prompt_tokens + reply.completion_tokens);
                let _ = self.queue.settle(&hold, at, &job.workspace, spent).await;
                if spent.is_some() {
                    tick.timed(&job.workspace, at - call_started);
                }
                let reply = match answered {
                    Ok(reply) => reply,
                    Err(ModelFailure::NoModel) => {
                        return Finish {
                            retry_at: Some(now + Duration::hours(1)),
                            ..self.with_tally(JobStatus::SkippedNoModel, Some("no-model"), tally)
                        };
                    }
                    Err(ModelFailure::Failed(reason)) => {
                        let reason = format!("model-{reason}");
                        self.log_failure(job, &head, &reason, now, tick).await;
                        return self.failed(job, now, &reason, tally);
                    }
                    // Rate-limited: retried soon, without spending an attempt —
                    // a busy key is not this job's fault. What this run applied
                    // stays; the retry carries on from this chunk. The whole
                    // pool of the workspace waits with it.
                    Err(ModelFailure::Busy {
                        reason,
                        retry_after_secs,
                    }) => {
                        let reason = format!("model-{reason}");
                        let wait = retry_after_secs
                            .unwrap_or(BUSY_RETRY_SECS)
                            .clamp(60, MAX_BUSY_RETRY_SECS);
                        let until = at + Duration::seconds(wait as i64);
                        tick.rate_limited(&job.workspace, until);
                        if let Some(row) = tick.row(&job.workspace, 0) {
                            let _ = self.queue.save_pool(&job.workspace, &row, at).await;
                        }
                        self.log_failure(job, &head, &reason, now, tick).await;
                        return Finish {
                            retry_at: Some(until),
                            chunks_done: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                            refund_attempt: true,
                            ..self.with_tally(JobStatus::Failed, Some(&reason), tally)
                        };
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
                            ..self.with_tally(
                                JobStatus::WaitingRunner,
                                Some("waiting-runner"),
                                tally,
                            )
                        };
                    }
                };
                tally.prompt = tally.prompt.saturating_add(reply.prompt_tokens);
                tally.completion = tally.completion.saturating_add(reply.completion_tokens);

                let fault = match edits::parse(&reply.text) {
                    // Usable: something to apply, or nothing refused.
                    Ok(output) if !output.pages.is_empty() || output.problems.is_empty() => {
                        break Some(output);
                    }
                    Ok(output) => Unusable::Refused(output.problems),
                    Err(bad) => Unusable::Bad(bad),
                };
                unusable = Some((fault.clone(), reply.truncated, max_tokens));
                // A runner's answer is checked before it is accepted; it is not
                // repaired here.
                if round > 0 || reply.runner {
                    break None;
                }
                round += 1;
                let (note, limit) = repair_note(&fault, reply.truncated, max_tokens);
                ask = format!("{user}{note}");
                max_tokens = limit;
                // The larger limit only if the budget has room; else the same.
                if spent_before.saturating_add(tally.tokens()).saturating_add(
                    prompt::estimate_tokens(prompt::SYSTEM)
                        + prompt::estimate_tokens(&ask)
                        + u64::from(max_tokens),
                ) > settings.job_tokens
                {
                    max_tokens = settings.max_output_tokens;
                }
            };
            let Some(output) = output else {
                tally.rejected += 1;
                let reason = unusable.take().map_or_else(
                    || "bad-output".to_owned(),
                    |(fault, cut, limit)| bad_output_reason(&fault, cut, limit),
                );
                if tally.pages.is_empty() {
                    self.log_failure(job, &head, &reason, now, tick).await;
                    return self.failed(job, now, &reason, tally);
                }
                break;
            };
            tally.rejected += output.refused;
            for edit in &output.pages {
                // Find/replace edits become the whole body here, over the
                // version the model was shown; one that does not apply is
                // refused like any other bad edit.
                let resolved;
                let edit = if edit.replacements.is_empty() {
                    edit
                } else {
                    let base = shown
                        .iter()
                        .find(|page| page.slug == edit.slug && page.version == edit.base_version);
                    match base.map(|page| edits::resolve(edit, &page.body)) {
                        Some(Ok(body)) => {
                            resolved = PageEdit {
                                body,
                                replacements: Vec::new(),
                                ..edit.clone()
                            };
                            &resolved
                        }
                        _ => {
                            tally.rejected += 1;
                            continue;
                        }
                    }
                };
                match self
                    .apply(&job.scope, &allowed, &shown, edit, &head, now, tick)
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
        if self.append_log(&job.scope, &line, tick).await {
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

    /// Appends a failed compile to the scope's log page, so the reason is
    /// in the wiki and not only in the queue — once per kind of failure
    /// (`model-http 429`, the part before the provider's body, which names
    /// a fresh request id every time), so a job retried every few minutes
    /// for the same 429 writes one line.
    async fn log_failure(
        &self,
        job: &Job,
        head: &SessionHead,
        reason: &str,
        now: OffsetDateTime,
        tick: &Tick,
    ) {
        let kind = |text: &str| text.split(':').next().unwrap_or_default().trim().to_owned();
        if job.last_error.as_deref().map(kind) == Some(kind(reason)) {
            return;
        }
        let short: String = head.session_id.chars().take(8).collect();
        let line = format!(
            "- **{}** · {}/{} · session `{short}` of {} · not compiled (attempt {}): {} · [source]({})",
            utc_day(now),
            head.agent_folder,
            head.project,
            head.day,
            job.attempts,
            edits::one_line(&redacted_text(reason), 400),
            prompt::source_href(&head.source_id)
        );
        let _ = self.append_log(&job.scope, &line, tick).await;
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
            triage: None,
            estimate: None,
        }
    }

    /// The triage plan for one claimed job.
    ///
    /// - "Compile fully" (`force_full`): full, no compaction, every page.
    /// - A compile resuming after its runner answered: the plan it started
    ///   with, so the transcript is cut exactly as before.
    /// - Otherwise the free rule (when triage is on), then — when a
    ///   classifier is wired — one call that may raise or (when sure) lower
    ///   the decision, pick the topic pages and mark outputs to keep or cut.
    async fn plan(
        &self,
        job: &Job,
        session: &Session,
        signals: &triage::Signals,
        allowed: &Allowed,
        settings: &Settings,
    ) -> CompilePlan {
        if job.force_full {
            return CompilePlan::full("forced", false);
        }
        if job.chunks_done > 0 {
            return job
                .triage_plan
                .as_deref()
                .and_then(CompilePlan::restore)
                .unwrap_or_else(|| CompilePlan::full("resumed", false));
        }
        let rule = if settings.triage {
            triage::rule(signals, settings.strictness)
        } else {
            Decision::new(Triage::Full, "triage-off")
        };
        let mut plan = Plan::from_rule(rule.clone());
        let classifier = match &self.classifiers {
            Some(classifiers) if settings.triage || settings.compaction => {
                classifiers.for_workspace(&job.workspace).await
            }
            _ => None,
        };
        if let Some(classifier) = &classifier {
            let candidates = self
                .topic_candidates(&job.scope, allowed)
                .await
                .unwrap_or_default();
            plan = plan
                .ask(
                    classifier.as_ref(),
                    session,
                    signals,
                    settings.strictness,
                    &candidates,
                )
                .await;
        }
        if !settings.triage {
            // Triage off: the classifier only informs compaction.
            plan.decision = rule;
            plan.topics = None;
        }
        CompilePlan {
            decision: plan.decision,
            compact: settings.compaction,
            keep: if settings.compaction {
                plan.keep
            } else {
                BTreeMap::new()
            },
            topics: plan.topics,
        }
    }

    /// The session's own topic pages — the pages in its folder beyond the
    /// fixed ones — with a title from the slug, for the classifier.
    async fn topic_candidates(
        &self,
        scope: &str,
        allowed: &Allowed,
    ) -> Result<Vec<(String, String)>, PageError> {
        let fixed = allowed.context_slugs();
        let prefix = allowed.folder_prefix();
        Ok(self
            .pages
            .list(&[scope], 1_000)
            .await?
            .into_iter()
            .filter(|page| page.slug.starts_with(&prefix) && !fixed.contains(&page.slug))
            .take(triage::MAX_TOPIC_QUESTIONS)
            .map(|page| {
                let title = page.slug[prefix.len()..].replace('-', " ");
                (page.slug, title)
            })
            .collect())
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
    ///
    /// With `topics` (the triage classifier's picks), the session's own
    /// topic pages are only those it picked; the fixed pages are always
    /// shown.
    async fn context(
        &self,
        scope: &str,
        allowed: &Allowed,
        topics: Option<&[String]>,
    ) -> Result<Vec<Shown>, PageError> {
        let listed = self.pages.list(&[scope], 1_000).await?;
        let mut slugs = allowed.context_slugs();
        let prefix = allowed.folder_prefix();
        for summary in &listed {
            if slugs.len() >= 24 {
                break;
            }
            let picked = topics.is_none_or(|topics| topics.contains(&summary.slug));
            let extra = (summary.slug.starts_with(&prefix) && picked)
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

    /// Writes one edit, if the session may write it.
    ///
    /// When the page moved on since the version the model was shown —
    /// another compile wrote it, in this drain or in another — the edit is
    /// merged onto the page as it is now ([`crate::merge`]) and written over
    /// that, re-read and merged again if it moves once more, a few times at
    /// most. Two compiles of one page take turns on it ([`Tick::page`]), so
    /// inside one drain the read, the merge and the write are one step. A
    /// page a *person* edited since the model's version is a conflict, as it
    /// always was: the next run shows the model their edit rather than
    /// merging around it.
    #[allow(clippy::too_many_arguments)]
    async fn apply(
        &self,
        scope: &str,
        allowed: &Allowed,
        shown: &[Shown],
        edit: &PageEdit,
        head: &SessionHead,
        now: OffsetDateTime,
        tick: &Tick,
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
        let Ok((body, _)) = redact(&strip_frontmatter(&edit.body), Policy::Redact) else {
            return Applied::Rejected;
        };
        let ours = with_citation(&fix_links(&body), head);
        let title = edit
            .title
            .as_deref()
            .map(redacted_line)
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| default_title(&place));
        let _turn = tick.page(scope, &edit.slug).await;
        // The body the model was shown: the base a merge replays it over.
        let mut base_body: Option<String> = None;
        for _ in 0..MERGE_ATTEMPTS {
            let current: Option<Page> = match self.pages.read(scope, &edit.slug).await {
                Ok(page) => page,
                Err(_) => return Applied::Conflict,
            };
            let at = current.as_ref().map(|page| page.version);
            let body = if at == edit.base_version {
                ours.clone()
            } else {
                // Moved on. Merged only over the brain's own versions, and
                // only forward from the one the model saw.
                let Some(head_version) = at else {
                    return Applied::Conflict;
                };
                let since = edit.base_version.unwrap_or(0);
                if since > head_version
                    || !self
                        .brain_only_since(scope, &edit.slug, since, head_version)
                        .await
                {
                    return Applied::Conflict;
                }
                if base_body.is_none() {
                    base_body = Some(match edit.base_version {
                        None => String::new(),
                        Some(version) => match shown
                            .iter()
                            .find(|page| page.slug == edit.slug && page.version == Some(version))
                        {
                            Some(page) => page.body.clone(),
                            None => match self.pages.read_version(scope, &edit.slug, version).await
                            {
                                Ok(Some(page)) => strip_frontmatter(&page.markdown),
                                _ => return Applied::Conflict,
                            },
                        },
                    });
                }
                let theirs = current
                    .as_ref()
                    .map(|page| strip_frontmatter(&page.markdown))
                    .unwrap_or_default();
                merge::three_way(base_body.as_deref().unwrap_or_default(), &theirs, &ours)
            };
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
            let mut lost_race = false;
            for _ in 0..2 {
                let write = PageWrite {
                    entity_type: EntityType::Topic,
                    markdown: markdown.clone(),
                    author: Author::Brain { reconciles },
                    base_version: at,
                };
                match self.pages.write(scope, &edit.slug, write).await {
                    Ok(_) => return Applied::Written,
                    // A person edited the page, and the model was shown that
                    // edit (it is at or before the version the model saw):
                    // the body already incorporates it, so the brain may say
                    // so.
                    Err(PageError::HumanEditPending { version })
                        if edit.base_version.is_some_and(|seen| version <= seen)
                            && reconciles.is_none() =>
                    {
                        reconciles = Some(version);
                    }
                    // Another writer, outside this drain, won the race:
                    // read again and merge onto theirs.
                    Err(PageError::Conflict { .. }) => {
                        lost_race = true;
                        break;
                    }
                    Err(PageError::HumanEditPending { .. }) => return Applied::Conflict,
                    Err(_) => return Applied::Rejected,
                }
            }
            if !lost_race {
                return Applied::Conflict;
            }
        }
        Applied::Conflict
    }

    /// Whether every version of the page after `since`, up to `head`, was
    /// written by the brain (another compile), not by a person. A history
    /// that cannot be read is not.
    async fn brain_only_since(&self, scope: &str, slug: &str, since: u32, head: u32) -> bool {
        let Ok(history) = self.pages.history(scope, slug).await else {
            return false;
        };
        history
            .iter()
            .filter(|meta| meta.version > since && meta.version <= head)
            .all(|meta| meta.author_kind == AuthorKind::Brain)
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
    async fn append_log(&self, scope: &str, line: &str, tick: &Tick) -> bool {
        // Compiles running side by side append to the same log: one at a time.
        let _turn = tick.page(scope, layout::LOG).await;
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

/// How many times one edit is merged onto a page that keeps moving before
/// it counts as a conflict.
const MERGE_ATTEMPTS: usize = 3;

/// One running compile, as the drain awaits it: the workspace lane it ran
/// in, the claimed job, how it ended, and whether the end was recorded.
type Running<'a> =
    Pin<Box<dyn Future<Output = (usize, Job, Finish, Result<(), DbError>)> + Send + 'a>>;

/// One workspace's jobs in a drain, and how far the drain got with them.
struct Lane {
    workspace: String,
    jobs: VecDeque<Job>,
    /// Its settings, read when the drain first takes one of its jobs.
    settings: Option<Settings>,
    /// Jobs claimed (and so counted against its `batch`).
    started: usize,
    running: usize,
    /// Jobs that ended `done`.
    done: usize,
    /// Nothing more of it starts in this drain (its pool waits out a 429,
    /// its budget is held, or the tick has no time for it).
    closed: bool,
}

impl Lane {
    fn new(job: Job) -> Self {
        Self {
            workspace: job.workspace.clone(),
            jobs: VecDeque::from([job]),
            settings: None,
            started: 0,
            running: 0,
            done: 0,
            closed: false,
        }
    }
}

/// What the drain does with one job it read.
enum Admit {
    /// Parked without a call (no model, no budget, paused).
    Parked(Processed),
    /// Somebody else changed or claimed it.
    Gone,
    /// Not now: its lane stops for this drain, the job stays as it is.
    Later,
    /// The drain starts nothing more.
    Stop,
    /// Claimed: compile it.
    Run(Box<Job>),
}

/// A workspace's compile budget, right now.
enum Budget {
    Open,
    /// What is left is held by calls in flight.
    Held,
    /// Spent until then.
    Spent(OffsetDateTime),
}

/// The lane the next compile is taken from: one with a job left, under its
/// pool's limit and (in a cron drain) its `batch`, that has started the
/// fewest so far — the oldest such lane first.
fn next_lane(lanes: &[Lane], tick: &Tick, filter: &ClaimFilter) -> Option<usize> {
    lanes
        .iter()
        .enumerate()
        .filter(|(_, lane)| !lane.closed && !lane.jobs.is_empty())
        .filter(|(_, lane)| match &lane.settings {
            None => true,
            Some(settings) => {
                lane.running < tick.limit(&lane.workspace)
                    && (filter.workspace.is_some() || lane.started < settings.batch)
            }
        })
        .min_by_key(|(index, lane)| (lane.started, *index))
        .map(|(index, _)| index)
}

/// Why a model's answer could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unusable {
    /// No answer of the edit list's shape at all.
    Bad(edits::BadOutput),
    /// An edit list, but every page edit in it was refused (by path).
    Refused(Vec<String>),
}

/// The most refusal reasons a repair note or a status names.
const MAX_PROBLEMS_SHOWN: usize = 6;

impl Unusable {
    fn describe(&self) -> String {
        match self {
            Self::Bad(bad) => bad.to_string(),
            Self::Refused(problems) => {
                let mut shown: Vec<&str> = problems
                    .iter()
                    .take(MAX_PROBLEMS_SHOWN)
                    .map(String::as_str)
                    .collect();
                let more = problems.len().saturating_sub(MAX_PROBLEMS_SHOWN);
                let tail = format!("{more} more");
                if more > 0 {
                    shown.push(&tail);
                }
                format!("every page edit refused: {}", shown.join("; "))
            }
        }
    }
}

/// What a repair round appends to the request, and its completion limit:
/// for an answer cut off at the limit, twice the limit (up to the ceiling)
/// and a request for a much shorter answer; otherwise the same limit and
/// what the validator found.
fn repair_note(fault: &Unusable, truncated: bool, max_tokens: u32) -> (String, u32) {
    let cut = truncated || matches!(fault, Unusable::Bad(bad) if bad.unterminated());
    if cut {
        let limit = max_tokens.saturating_mul(2).min(MAX_OUTPUT_TOKENS_CEILING);
        return (
            format!(
                "\n\nREPAIR: your previous answer was cut off at the output limit ({max_tokens} tokens) before the JSON object closed, so none of it could be used. Answer again with the same JSON shape, much shorter: for every page that already exists use \"edits\" (exact find/replace pairs against the body shown above) instead of the whole \"body\"; include only the pages this session really changes; keep facts few. Reply with the JSON object only."
            ),
            limit,
        );
    }
    (
        format!(
            "\n\nREPAIR: your previous answer could not be used ({}). Reply again with one JSON object in exactly the OUTPUT shape, and nothing else: no prose, no code fence.",
            fault.describe()
        ),
        max_tokens,
    )
}

/// The `bad-output` status a compile fails with: what was wrong, and
/// whether the answer stopped at the output limit. Fixed wording plus
/// positions and field paths only — never the model's text.
fn bad_output_reason(fault: &Unusable, truncated: bool, max_tokens: u32) -> String {
    let what = fault.describe();
    let text = if truncated {
        format!("bad-output: cut off at the output limit ({max_tokens} tokens): {what}")
    } else {
        format!("bad-output: {what}")
    };
    edits::one_line(&text, 400)
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

/// How long a rate-limited job waits when the provider named no
/// `Retry-After`.
const BUSY_RETRY_SECS: u64 = 5 * 60;
/// The longest a rate-limited job waits, whatever `Retry-After` says.
const MAX_BUSY_RETRY_SECS: u64 = 30 * 60;

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

/// What a run decided before it compiled, recorded on its finish whichever
/// way it ends.
#[derive(Debug, Default)]
struct Stamp {
    triage: Option<(String, String, Option<String>)>,
    estimate: Option<(u64, u64)>,
}

/// How one compile runs: the triage decision, whether the transcript is
/// compacted and how, and the topic pages shown. Stored on the job (ids
/// and flags only) so a resumed compile cuts the transcript the same way.
#[derive(Debug, Clone, PartialEq)]
struct CompilePlan {
    decision: Decision,
    compact: bool,
    keep: BTreeMap<CallRef, Keep>,
    topics: Option<Vec<String>>,
}

impl CompilePlan {
    fn full(reason: &str, compact: bool) -> Self {
        Self {
            decision: Decision::new(Triage::Full, reason),
            compact,
            keep: BTreeMap::new(),
            topics: None,
        }
    }

    /// The stored form, for a full compile only (the others never resume).
    fn stored(&self) -> Option<String> {
        if self.decision.triage != Triage::Full {
            return None;
        }
        let keep: Vec<serde_json::Value> = self
            .keep
            .iter()
            .map(|((turn, call), keep)| {
                serde_json::json!([turn, call, if *keep == Keep::Verbatim { "v" } else { "s" }])
            })
            .collect();
        Some(
            serde_json::json!({
                "reason": self.decision.reason,
                "compact": self.compact,
                "keep": keep,
                "topics": self.topics,
            })
            .to_string(),
        )
    }

    fn restore(stored: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(stored).ok()?;
        let keep = value
            .get("keep")
            .and_then(serde_json::Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| {
                        let turn = usize::try_from(entry.get(0)?.as_u64()?).ok()?;
                        let call = usize::try_from(entry.get(1)?.as_u64()?).ok()?;
                        let keep = match entry.get(2)?.as_str()? {
                            "v" => Keep::Verbatim,
                            _ => Keep::SignalOnly,
                        };
                        Some(((turn, call), keep))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let topics = value.get("topics").and_then(|topics| {
            topics.as_array().map(|slugs| {
                slugs
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
        });
        Some(Self {
            decision: Decision::new(
                Triage::Full,
                value
                    .get("reason")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("resumed"),
            ),
            compact: value
                .get("compact")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            keep,
            topics,
        })
    }
}

/// The log line of a `summary-only` session: what it did, in counts, and
/// how to compile it after all.
fn summary_line(
    head: &SessionHead,
    signals: &triage::Signals,
    reason: &str,
    now: OffsetDateTime,
) -> String {
    let short: String = head.session_id.chars().take(8).collect();
    format!(
        "- **{}** · {}/{} · session `{short}` of {} · {} · not compiled — **Compile fully** on the queue compiles it · [source]({})",
        utc_day(now),
        head.agent_folder,
        head.project,
        head.day,
        edits::one_line(&redacted_text(&triage::summary_text(signals, reason)), 300),
        prompt::source_href(&head.source_id)
    )
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
