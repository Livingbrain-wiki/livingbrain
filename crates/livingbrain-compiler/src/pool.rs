//! One drain's shared state: the compiles it runs side by side, per
//! workspace, and what they must agree on while they run.
//!
//! - **The pool per workspace.** Each workspace's compiles share its model
//!   key, so they share its rate limit: one 429 pauses the whole pool for
//!   the provider's `Retry-After` (no compile of that workspace starts a
//!   call until then, in this drain or the next) and halves how many run at
//!   once; a drain that ran its pool full without a 429 raises it by one,
//!   up to the workspace's setting (`coding_agents.concurrency`). The pool,
//!   with a rolling average of how long one model call takes, is kept per
//!   workspace in `coding_agent_pool` between drains.
//! - **Time.** A cron drain starts a compile only while its budget's
//!   deadline leaves room for one more call of average length, and never
//!   starts a call that could outlast the invocation's wall: a compile that
//!   reaches it stops between two chunks and is queued again, carrying on
//!   later from the chunk it reached.
//! - **Pages.** Two compiles writing one page take turns on it
//!   ([`Tick::page`]) — the read, the merge and the write are one step — and
//!   the page store's `base_version` catches a writer outside this drain.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use cratefield_core::ScheduledBudget;
use futures_util::lock::{Mutex as AsyncMutex, OwnedMutexGuard};
use time::{Duration, OffsetDateTime};

use crate::queue::{PoolRow, rfc3339};

/// The most compiles one drain runs at once, across every workspace. A
/// Worker invocation keeps at most six outbound connections open at a time
/// (the rest wait for one to close), and each running compile holds at most
/// one: its model call.
pub const MAX_IN_FLIGHT: usize = 6;

/// The most compiles of one workspace a drain runs at once, whatever the
/// workspace or the deployment asks for (`coding_agents.concurrency`).
/// Above [`MAX_IN_FLIGHT`] on purpose: the setting is a ceiling on the
/// pool, and the invocation's connections are the tighter bound.
pub const CONCURRENCY_CEILING: usize = 8;

/// The longest one compile call may take: the composition's model-call
/// deadline (`livingbrain-api`'s `CALL_TIMEOUT`) must not be longer.
pub const MAX_CALL: Duration = Duration::minutes(8);

/// How far past the cron budget's deadline the invocation's own wall is.
/// A scheduled Worker runs for at most 15 minutes; the venture holds the
/// coding-agents share's deadline at most 5.5 minutes in (`module_budget`
/// in `livingbrain-venture`, whatever its wall), so the wall is at most
/// 9.5 minutes after it — 9 here, the rest kept for the finish and the lint.
pub const WALL_AFTER_BUDGET: Duration = Duration::minutes(9);

/// The average call length a workspace starts with before one was timed.
const DEFAULT_CALL: Duration = Duration::seconds(60);

/// Why a compile stops between two chunks and is queued again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Halt {
    /// The workspace's model said 429: the pool waits until then.
    Paused,
    /// The next call could outlast the invocation.
    Deadline,
}

impl Halt {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::Paused => "pool-backoff",
            Self::Deadline => "tick-deadline",
        }
    }
}

/// One workspace's pool, while a drain runs.
#[derive(Debug, Clone)]
struct Lane {
    /// How many of its compiles may run at once now (adaptive).
    limit: usize,
    /// Its setting: the most `limit` may grow to.
    ceiling: usize,
    /// No call starts before this (a 429's `Retry-After`).
    paused_until: Option<OffsetDateTime>,
    /// A stored pause this drain ignores (`Process now`), kept for the next.
    ignored_pause: Option<String>,
    /// A 429 was seen in this drain.
    rate_limited: bool,
    /// The rolling average of one model call, in milliseconds.
    avg_call_ms: u64,
    /// Calls the average was taken over (all time).
    calls: u64,
}

/// One drain's shared state. See the module docs.
pub(crate) struct Tick {
    /// Start no compile past this (estimated): the cron budget's deadline.
    deadline: Option<OffsetDateTime>,
    /// Start no call that could end past this: the invocation's wall.
    wall: Option<OffsetDateTime>,
    /// `Process now`: a stored pause does not hold this drain back.
    ignore_backoff: bool,
    lanes: Mutex<BTreeMap<String, Lane>>,
    pages: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
}

impl Tick {
    pub(crate) fn new(budget: &ScheduledBudget, ignore_backoff: bool) -> Self {
        let deadline = budget.deadline();
        Self {
            deadline,
            wall: deadline.map(|deadline| deadline + WALL_AFTER_BUDGET),
            ignore_backoff,
            lanes: Mutex::default(),
            pages: Mutex::default(),
        }
    }

    /// A workspace's pool for this drain, from what the last drain left
    /// (`row`) and its setting (`concurrency`).
    pub(crate) fn open(&self, workspace: &str, row: &PoolRow, concurrency: usize) {
        let ceiling = concurrency.clamp(1, CONCURRENCY_CEILING);
        let limit = row
            .concurrency
            .map_or(ceiling, |stored| usize::try_from(stored).unwrap_or(ceiling))
            .clamp(1, ceiling);
        let (paused_until, ignored_pause) = if self.ignore_backoff {
            (None, row.paused_until.clone())
        } else {
            (row.paused_until.as_deref().and_then(parse), None)
        };
        let lane = Lane {
            limit,
            ceiling,
            paused_until,
            ignored_pause,
            rate_limited: false,
            avg_call_ms: if row.calls == 0 {
                millis(DEFAULT_CALL)
            } else {
                row.avg_call_ms.max(1)
            },
            calls: row.calls,
        };
        self.lanes().insert(workspace.to_owned(), lane);
    }

    /// How many of the workspace's compiles may run at once now.
    pub(crate) fn limit(&self, workspace: &str) -> usize {
        self.lanes().get(workspace).map_or(1, |lane| lane.limit)
    }

    /// Whether a compile of `workspace` may start now: its pool is not
    /// paused, and (in a cron drain) the budget leaves room for one call of
    /// average length.
    pub(crate) fn may_start(&self, workspace: &str, now: OffsetDateTime) -> bool {
        let lanes = self.lanes();
        let Some(lane) = lanes.get(workspace) else {
            return true;
        };
        if lane.paused_until.is_some_and(|until| until > now) {
            return false;
        }
        self.deadline.is_none_or(|deadline| {
            now + Duration::milliseconds(i64::try_from(lane.avg_call_ms).unwrap_or(i64::MAX))
                <= deadline
        })
    }

    /// Why the next call of a `workspace` compile must not start now, if it
    /// must not.
    pub(crate) fn halt(&self, workspace: &str, now: OffsetDateTime) -> Option<Halt> {
        if self
            .lanes()
            .get(workspace)
            .and_then(|lane| lane.paused_until)
            .is_some_and(|until| until > now)
        {
            return Some(Halt::Paused);
        }
        if self.wall.is_some_and(|wall| now + MAX_CALL > wall) {
            return Some(Halt::Deadline);
        }
        None
    }

    /// The workspace's model refused a call (429) until `until`: the pool
    /// pauses until then and runs half as many at once.
    pub(crate) fn rate_limited(&self, workspace: &str, until: OffsetDateTime) {
        if let Some(lane) = self.lanes().get_mut(workspace) {
            lane.paused_until = Some(lane.paused_until.map_or(until, |was| was.max(until)));
            lane.limit = (lane.limit / 2).max(1);
            lane.rate_limited = true;
        }
    }

    /// One model call of `workspace` took `took`.
    pub(crate) fn timed(&self, workspace: &str, took: Duration) {
        if let Some(lane) = self.lanes().get_mut(workspace) {
            let ms = millis(took).max(1);
            lane.avg_call_ms = if lane.calls == 0 {
                ms
            } else {
                // A rolling average: the last few calls weigh the most.
                (lane.avg_call_ms * 7 + ms * 3) / 10
            };
            lane.calls = lane.calls.saturating_add(1);
        }
    }

    /// The pool as the next drain should find it. A drain that ran
    /// `completed` compiles of the workspace — at least its whole pool —
    /// without a 429 raises the pool by one, up to the setting.
    pub(crate) fn row(&self, workspace: &str, completed: usize) -> Option<PoolRow> {
        let lanes = self.lanes();
        let lane = lanes.get(workspace)?;
        let limit = if !lane.rate_limited && completed >= lane.limit {
            (lane.limit + 1).min(lane.ceiling)
        } else {
            lane.limit
        };
        Some(PoolRow {
            concurrency: Some(u64::try_from(limit).unwrap_or(1)),
            paused_until: lane
                .paused_until
                .map(rfc3339)
                .or_else(|| lane.ignored_pause.clone()),
            avg_call_ms: lane.avg_call_ms,
            calls: lane.calls,
        })
    }

    /// The page `slug` of `scope`, held by this compile until the guard is
    /// dropped: the read, the merge and the write of one page are one step.
    pub(crate) async fn page(&self, scope: &str, slug: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut pages = self
                .pages
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            Arc::clone(pages.entry(format!("{scope}\u{0}{slug}")).or_default())
        };
        lock.lock_owned().await
    }

    fn lanes(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Lane>> {
        self.lanes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.whole_milliseconds()).unwrap_or(0)
}

fn parse(stamp: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339).ok()
}
