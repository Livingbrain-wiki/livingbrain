//! The queue for source material: GitHub items, Discord channel-days and
//! imported files, grouped into **units** and compiled into pages.
//!
//! A unit is the batch one compile call family works on:
//!
//! - `github` — one repository's day: the commits, pull requests and issues
//!   of that repository stored on that UTC day (`unit_day` is the day the
//!   ledger filed them, which for a live sync is the day they happened).
//! - `discord` — one channel-day, which is how Discord already arrives.
//! - `imports` — one imported file; a large file is cut into pieces and
//!   compiled over several calls.
//!
//! **Enqueueing is a bounded backlog pass.** [`Queue::enqueue_sources`]
//! finds up to a fixed number of stored sources that have no member row,
//! oldest first, and files each under its unit. Its key `(scope, source_id)`
//! makes a second pass a no-op, so the pre-existing backlog is queued once
//! and live sources are picked up by the same pass on the next tick.
//!
//! **Members remember their progress.** A member is a source of a unit with
//! `pieces_done` of `pieces_total` pieces compiled; a source that arrives
//! for a unit already compiled reopens the unit (`done` → `queued`), and the
//! next run compiles only the members still pending.
//!
//! **Claims are compare-and-set** exactly like the session queue's: one
//! `UPDATE` that names the status and `updated_at` it was read at, under a
//! lease, so a Worker that died mid-unit leaves a unit that becomes
//! claimable again.

use cratefield_core::{DbError, Row, Statement};
use sea_query::Value as SeaValue;
use std::collections::BTreeMap;
use time::{Duration, OffsetDateTime};

use crate::queue::{JobStatus, MAX_ATTEMPTS, Queue, rfc3339, utc_day};

/// What one backlog pass did: how many stored sources it scanned after the
/// cursor, and how many of them it filed under a unit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Filed {
    pub scanned: usize,
    pub filed: usize,
}

/// The lane a GitHub unit belongs to (its pages' agent folder is `github`).
pub const LANE_GITHUB: &str = "github";
/// The lane of Discord channel-days.
pub const LANE_DISCORD: &str = "discord";
/// The lane of imported files.
pub const LANE_IMPORTS: &str = "imports";

/// The source kinds a backlog pass files, as their stored names.
pub const UNIT_KINDS: [&str; 5] = [
    "github_commit",
    "github_issue",
    "github_pr",
    "discord",
    "import",
];

/// The most sources one backlog pass scans: a bounded read after the cursor.
/// A ledger of N stored sources is read once, in about N / 200 cron ticks.
pub const BACKLOG_PER_TICK: usize = 200;

/// The most members one unit run reads: a run compiles at most its
/// `max_chunks` calls, and the rest of a larger unit waits for the next.
pub const MEMBERS_PER_RUN: usize = 200;

/// One unit as the drain reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    pub scope: String,
    pub unit_key: String,
    pub workspace: String,
    /// The member the model call runs for (the one who stored the source).
    pub owner: String,
    pub lane: String,
    pub unit_day: String,
    /// The project folder the unit's pages go under.
    pub label: String,
    pub status: JobStatus,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub pages: Vec<String>,
    pub rejected: u32,
    pub facts: u32,
    pub queued_at: String,
    pub updated_at: String,
    pub lease_until: Option<String>,
    pub next_attempt_at: Option<String>,
}

/// How a unit run ended, as the queue records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFinish {
    pub status: JobStatus,
    pub error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub pages: Vec<String>,
    pub rejected: u32,
    pub facts: u32,
    pub retry_at: Option<OffsetDateTime>,
    /// Whether the attempt this run counted is given back (the run ended by
    /// budget or by a wait, not by a fault).
    pub refund_attempt: bool,
}

/// A member of a unit the run has not finished compiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub source_id: String,
    /// Pieces of this source already compiled.
    pub pieces_done: usize,
    /// The ledger's time for the source (RFC 3339 for a stored row).
    pub created_at: String,
}

/// Per-lane counts for the status surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaneCounts {
    /// Units by status.
    pub units: BTreeMap<JobStatus, u64>,
    /// Stored sources of the lane not compiled yet.
    pub sources_pending: u64,
    /// Stored sources of the lane compiled.
    pub sources_compiled: u64,
}

/// A lane's name and the day and project a stored source files under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    pub unit_key: String,
    pub lane: &'static str,
    pub unit_day: String,
}

/// Where one stored source goes: its unit key and lane. `None` for a kind
/// that is not compiled here.
#[must_use]
pub fn placement(
    kind: &str,
    source_id: &str,
    rel_path: &str,
    created_at: &str,
) -> Option<Placement> {
    let day = utc_day_of(created_at);
    match kind {
        "github_commit" | "github_issue" | "github_pr" => {
            // `github/<owner>/<repo>/<leaf>`, as the GitHub app files it.
            let repo = rel_path
                .strip_prefix("github/")
                .and_then(|rest| {
                    let mut parts = rest.split('/');
                    let owner = parts.next()?;
                    let name = parts.next()?;
                    (!owner.is_empty() && !name.is_empty()).then(|| format!("{owner}/{name}"))
                })
                .unwrap_or_else(|| "unknown".to_owned());
            Some(Placement {
                unit_key: format!("github:{repo}:{day}"),
                lane: LANE_GITHUB,
                unit_day: day,
            })
        }
        "discord" => Some(Placement {
            unit_key: format!("discord:{source_id}"),
            lane: LANE_DISCORD,
            unit_day: day,
        }),
        "import" => Some(Placement {
            unit_key: format!("import:{source_id}"),
            lane: LANE_IMPORTS,
            unit_day: day,
        }),
        _ => None,
    }
}

/// `YYYY-MM-DD` of a stored timestamp (its first ten characters), or the
/// empty string when it has none.
fn utc_day_of(created_at: &str) -> String {
    created_at.get(..10).unwrap_or_default().to_owned()
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn big(value: u64) -> SeaValue {
    SeaValue::BigInt(Some(i64::try_from(value).unwrap_or(i64::MAX)))
}

fn opt_text(value: Option<&str>) -> SeaValue {
    match value {
        Some(value) => text(value),
        None => SeaValue::String(None),
    }
}

const UNIT_COLUMNS: &str = "scope, unit_key, workspace, owner, lane, unit_day, label, status, \
     attempts, last_error, prompt_tokens, completion_tokens, pages, rejected, facts, queued_at, \
     updated_at, lease_until, next_attempt_at";

fn unit_from(row: &Row) -> Option<Unit> {
    let status = JobStatus::parse(&row.get::<String>("status")?)?;
    let pages: Vec<String> = row
        .get::<String>("pages")
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    let count = |name: &str| {
        row.get::<i64>(name)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0)
    };
    Some(Unit {
        scope: row.get::<String>("scope")?,
        unit_key: row.get::<String>("unit_key")?,
        workspace: row.get::<String>("workspace")?,
        owner: row.get::<String>("owner").unwrap_or_default(),
        lane: row.get::<String>("lane")?,
        unit_day: row.get::<String>("unit_day").unwrap_or_default(),
        label: row.get::<String>("label")?,
        status,
        attempts: u32::try_from(count("attempts")).unwrap_or(u32::MAX),
        last_error: row.get::<String>("last_error"),
        prompt_tokens: count("prompt_tokens"),
        completion_tokens: count("completion_tokens"),
        pages,
        rejected: u32::try_from(count("rejected")).unwrap_or(u32::MAX),
        facts: u32::try_from(count("facts")).unwrap_or(u32::MAX),
        queued_at: row.get::<String>("queued_at").unwrap_or_default(),
        updated_at: row.get::<String>("updated_at").unwrap_or_default(),
        lease_until: row.get::<String>("lease_until"),
        next_attempt_at: row.get::<String>("next_attempt_at"),
    })
}

impl Queue {
    /// The backlog pass: scans up to `limit` stored sources after the
    /// cursor (the last id a pass scanned), files the ones of [`UNIT_KINDS`]
    /// under their units, and moves the cursor past everything it scanned.
    /// A source is read once, not once per tick. Held sources are filed too
    /// (their members wait for the release, see [`Queue::pending_members_of`]),
    /// and a `done` unit that gained a releasable member reopens here.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn enqueue_sources(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Filed, DbError> {
        let stamp = rfc3339(now);
        let mut report = Filed::default();
        if limit > 0 {
            let cursor = self.source_cursor().await?;
            let rows = self
                .db
                .query(&Statement::with_values(
                    "SELECT id, scope, workspace, kind, rel_path, project, imported_by, created_at \
                     FROM sources WHERE id > ? ORDER BY id ASC LIMIT ?",
                    vec![text(cursor.as_deref().unwrap_or("")), big(limit as u64)],
                ))
                .await?;
            let mut last: Option<String> = None;
            for row in &rows.rows {
                report.scanned += 1;
                let (Some(id), Some(scope), Some(workspace), Some(kind), Some(created)) = (
                    row.get::<String>("id"),
                    row.get::<String>("scope"),
                    row.get::<String>("workspace"),
                    row.get::<String>("kind"),
                    row.get::<String>("created_at"),
                ) else {
                    continue;
                };
                last = Some(id.clone());
                if !UNIT_KINDS.contains(&kind.as_str()) {
                    continue;
                }
                let rel_path = row.get::<String>("rel_path").unwrap_or_default();
                let project = row.get::<String>("project");
                let owner = row.get::<String>("imported_by").unwrap_or_default();
                let Some(place) = placement(&kind, &id, &rel_path, &created) else {
                    continue;
                };
                let label = unit_label(place.lane, &rel_path, project.as_deref());
                self.db
                    .execute(&Statement::with_values(
                        "INSERT INTO source_compile_units \
                         (scope, unit_key, workspace, owner, lane, unit_day, label, status, \
                          queued_at, updated_at) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?) \
                         ON CONFLICT (scope, unit_key) DO NOTHING",
                        vec![
                            text(&scope),
                            text(&place.unit_key),
                            text(&workspace),
                            text(&owner),
                            text(place.lane),
                            text(&place.unit_day),
                            text(&label),
                            text(&stamp),
                            text(&stamp),
                        ],
                    ))
                    .await?;
                let joined = self
                    .db
                    .execute(&Statement::with_values(
                        "INSERT INTO source_compile_members \
                         (scope, source_id, unit_key, pieces_total, pieces_done, compiled_at, \
                          source_created_at) \
                         VALUES (?, ?, ?, 0, 0, NULL, ?) \
                         ON CONFLICT (scope, source_id) DO NOTHING",
                        vec![
                            text(&scope),
                            text(&id),
                            text(&place.unit_key),
                            text(&created),
                        ],
                    ))
                    .await?;
                report.filed += joined as usize;
            }
            if let Some(last) = last {
                self.db
                    .execute(&Statement::with_values(
                        "INSERT INTO source_compile_cursor (name, last_id, updated_at) \
                         VALUES ('sources', ?, ?) \
                         ON CONFLICT (name) DO UPDATE SET last_id = excluded.last_id, \
                                updated_at = excluded.updated_at",
                        vec![text(&last), text(&stamp)],
                    ))
                    .await?;
            }
        }
        // A released source's member is pending again: its done unit reopens.
        self.db
            .execute(&Statement::with_values(
                "UPDATE source_compile_units SET status = 'queued', next_attempt_at = NULL, \
                        updated_at = ? \
                 WHERE status = 'done' AND EXISTS (\
                     SELECT 1 FROM source_compile_members m \
                     JOIN sources s ON s.scope = m.scope AND s.id = m.source_id \
                     WHERE m.scope = source_compile_units.scope \
                       AND m.unit_key = source_compile_units.unit_key \
                       AND m.compiled_at IS NULL AND s.held = 0)",
                vec![text(&stamp)],
            ))
            .await?;
        Ok(report)
    }

    /// The backlog pass's cursor: the last source id it scanned, or `None`
    /// before its first pass.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn source_cursor(&self) -> Result<Option<String>, DbError> {
        let rows = self
            .db
            .query(&Statement::new(
                "SELECT last_id FROM source_compile_cursor WHERE name = 'sources'",
            ))
            .await?;
        Ok(rows.first().and_then(|row| row.get::<String>("last_id")))
    }

    /// Tokens the source units of `workspace` spent on UTC days `first`
    /// through `last` (settled calls only).
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn unit_spent(
        &self,
        workspace: &str,
        first: &str,
        last: &str,
    ) -> Result<u64, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT tokens FROM source_compile_spend WHERE workspace = ? AND day >= ? AND day <= ?",
                vec![text(workspace), text(first), text(last)],
            ))
            .await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.get::<i64>("tokens"))
            .filter_map(|n| u64::try_from(n).ok())
            .sum())
    }

    /// Tokens held by the source units' calls in flight on UTC days `first`
    /// through `last` (their holds, not yet settled or lapsed).
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn unit_held(
        &self,
        workspace: &str,
        first: &str,
        last: &str,
        now: OffsetDateTime,
    ) -> Result<u64, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT tokens FROM coding_agent_reservations \
                 WHERE workspace = ? AND day >= ? AND day <= ? AND expires_at > ? \
                   AND id LIKE 'source-hold:%'",
                vec![
                    text(workspace),
                    text(first),
                    text(last),
                    text(&rfc3339(now)),
                ],
            ))
            .await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| row.get::<i64>("tokens"))
            .filter_map(|n| u64::try_from(n).ok())
            .sum())
    }

    /// Records the tokens one settled unit call spent, on its day.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn record_unit_spend(
        &self,
        workspace: &str,
        now: OffsetDateTime,
        tokens: u64,
    ) -> Result<(), DbError> {
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO source_compile_spend (workspace, day, tokens) VALUES (?, ?, ?) \
                 ON CONFLICT (workspace, day) DO UPDATE SET \
                        tokens = source_compile_spend.tokens + excluded.tokens",
                vec![text(workspace), text(&utc_day(now)), big(tokens)],
            ))
            .await?;
        Ok(())
    }

    /// The units a drain may take now, oldest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn ready_units(
        &self,
        now: OffsetDateTime,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Unit>, DbError> {
        let stamp = rfc3339(now);
        let mut values = vec![
            text(&stamp),
            text(&stamp),
            SeaValue::BigInt(Some(i64::from(MAX_ATTEMPTS))),
            text(&stamp),
        ];
        let mut sql = format!(
            "SELECT {UNIT_COLUMNS} FROM source_compile_units WHERE \
             (status = 'queued' \
              OR (status IN ('skipped-no-model', 'over-budget', 'waiting-runner') \
                  AND next_attempt_at <= ?) \
              OR (status = 'failed' AND next_attempt_at <= ? AND attempts < ?) \
              OR (status = 'processing' AND lease_until < ?))"
        );
        if let Some(workspace) = workspace {
            sql.push_str(" AND workspace = ?");
            values.push(text(workspace));
        }
        sql.push_str(" ORDER BY queued_at ASC, scope ASC, unit_key ASC LIMIT ?");
        values.push(big(limit as u64));
        let rows = self.db.query(&Statement::with_values(sql, values)).await?;
        Ok(rows.rows.iter().filter_map(unit_from).collect())
    }

    /// One unit, or `None`.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn unit(&self, scope: &str, unit_key: &str) -> Result<Option<Unit>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                format!("SELECT {UNIT_COLUMNS} FROM source_compile_units WHERE scope = ? AND unit_key = ?"),
                vec![text(scope), text(unit_key)],
            ))
            .await?;
        Ok(rows.first().and_then(unit_from))
    }

    /// Claims a unit read by [`Queue::ready_units`]: `processing` under a
    /// lease, one attempt counted. `false` when it changed since it was read.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn claim_unit(
        &self,
        unit: &Unit,
        now: OffsetDateTime,
        lease: Duration,
    ) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE source_compile_units SET status = 'processing', attempts = attempts + 1, \
                        lease_until = ?, updated_at = ? \
                 WHERE scope = ? AND unit_key = ? AND status = ? AND updated_at = ?",
                vec![
                    text(&rfc3339(now + lease)),
                    text(&rfc3339(now)),
                    text(&unit.scope),
                    text(&unit.unit_key),
                    text(unit.status.as_str()),
                    text(&unit.updated_at),
                ],
            ))
            .await?;
        Ok(changed == 1)
    }

    /// Parks an unread-to-claim unit (no model, no budget, paused) without
    /// counting an attempt. Compare-and-set like a claim.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn park_unit(
        &self,
        unit: &Unit,
        status: JobStatus,
        now: OffsetDateTime,
        retry_at: OffsetDateTime,
        reason: &str,
    ) -> Result<bool, DbError> {
        let changed = self
            .db
            .execute(&Statement::with_values(
                "UPDATE source_compile_units SET status = ?, last_error = ?, next_attempt_at = ?, \
                        lease_until = NULL, updated_at = ? \
                 WHERE scope = ? AND unit_key = ? AND status = ? AND updated_at = ?",
                vec![
                    text(status.as_str()),
                    text(reason),
                    text(&rfc3339(retry_at)),
                    text(&rfc3339(now)),
                    text(&unit.scope),
                    text(&unit.unit_key),
                    text(unit.status.as_str()),
                    text(&unit.updated_at),
                ],
            ))
            .await?;
        Ok(changed == 1)
    }

    /// The members of a unit not compiled yet, oldest first, at most `limit`.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn pending_members_of(
        &self,
        scope: &str,
        unit_key: &str,
        limit: usize,
    ) -> Result<Vec<Member>, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT m.source_id AS source_id, m.pieces_done AS pieces_done, \
                        m.source_created_at AS source_created_at \
                 FROM source_compile_members m \
                 JOIN sources s ON s.scope = m.scope AND s.id = m.source_id \
                 WHERE m.scope = ? AND m.unit_key = ? AND m.compiled_at IS NULL AND s.held = 0 \
                 ORDER BY m.source_created_at ASC, m.source_id ASC LIMIT ?",
                vec![text(scope), text(unit_key), big(limit as u64)],
            ))
            .await?;
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| {
                Some(Member {
                    source_id: row.get::<String>("source_id")?,
                    pieces_done: row
                        .get::<i64>("pieces_done")
                        .and_then(|n| usize::try_from(n).ok())
                        .unwrap_or(0),
                    created_at: row.get::<String>("source_created_at").unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Records that `pieces_done` of a member's `pieces_total` pieces are
    /// compiled; the member is compiled once all of them are.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn advance_member(
        &self,
        scope: &str,
        source_id: &str,
        pieces_total: usize,
        pieces_done: usize,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        let compiled = (pieces_done >= pieces_total).then(|| rfc3339(now));
        self.db
            .execute(&Statement::with_values(
                "UPDATE source_compile_members SET pieces_total = ?, pieces_done = ?, \
                        compiled_at = COALESCE(?, compiled_at) \
                 WHERE scope = ? AND source_id = ?",
                vec![
                    big(pieces_total as u64),
                    big(pieces_done as u64),
                    opt_text(compiled.as_deref()),
                    text(scope),
                    text(source_id),
                ],
            ))
            .await?;
        Ok(())
    }

    /// How many members of a unit are not compiled yet.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn pending_members(&self, scope: &str, unit_key: &str) -> Result<u64, DbError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT COUNT(*) AS n FROM source_compile_members m \
                 JOIN sources s ON s.scope = m.scope AND s.id = m.source_id \
                 WHERE m.scope = ? AND m.unit_key = ? AND m.compiled_at IS NULL AND s.held = 0",
                vec![text(scope), text(unit_key)],
            ))
            .await?;
        Ok(rows
            .first()
            .and_then(|row| row.get::<i64>("n"))
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0))
    }

    /// Records how a claimed unit ended. Tokens, facts and rejections
    /// accumulate across runs; pages accumulate as a set.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn finish_unit(
        &self,
        unit: &Unit,
        finish: &UnitFinish,
        now: OffsetDateTime,
    ) -> Result<(), DbError> {
        let mut pages = unit.pages.clone();
        for slug in &finish.pages {
            if !pages.contains(slug) {
                pages.push(slug.clone());
            }
        }
        let pages_json = serde_json::to_string(&pages).unwrap_or_else(|_| "[]".to_owned());
        // A run that got through its calls clears the attempt count; one
        // that ended by budget or a wait gives its attempt back.
        let attempts = if finish.status == JobStatus::Failed {
            unit.attempts
        } else if finish.refund_attempt {
            unit.attempts.saturating_sub(1)
        } else {
            0
        };
        self.db
            .execute(&Statement::with_values(
                "UPDATE source_compile_units SET status = ?, last_error = ?, \
                        prompt_tokens = prompt_tokens + ?, completion_tokens = completion_tokens + ?, \
                        pages = ?, rejected = rejected + ?, facts = facts + ?, lease_until = NULL, \
                        next_attempt_at = ?, updated_at = ?, attempts = ? \
                 WHERE scope = ? AND unit_key = ?",
                vec![
                    text(finish.status.as_str()),
                    opt_text(finish.error.as_deref()),
                    big(finish.prompt_tokens),
                    big(finish.completion_tokens),
                    text(&pages_json),
                    big(u64::from(finish.rejected)),
                    big(u64::from(finish.facts)),
                    opt_text(finish.retry_at.map(rfc3339).as_deref()),
                    text(&rfc3339(now)),
                    big(u64::from(attempts)),
                    text(&unit.scope),
                    text(&unit.unit_key),
                ],
            ))
            .await?;
        Ok(())
    }

    /// Per-lane unit and source counts over the asker's scopes of
    /// `workspace`. Same two clauses as every other listing.
    ///
    /// # Errors
    ///
    /// [`DbError`] from the database.
    pub async fn unit_counts(
        &self,
        workspace: &str,
        scopes: &[String],
    ) -> Result<BTreeMap<String, LaneCounts>, DbError> {
        let mut lanes: BTreeMap<String, LaneCounts> = BTreeMap::new();
        if scopes.is_empty() {
            return Ok(lanes);
        }
        let marks = vec!["?"; scopes.len()].join(", ");
        let mut values = vec![text(workspace)];
        values.extend(scopes.iter().map(|scope| text(scope)));
        let units = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT lane, status, COUNT(*) AS n FROM source_compile_units \
                     WHERE workspace = ? AND scope IN ({marks}) GROUP BY lane, status"
                ),
                values.clone(),
            ))
            .await?;
        for row in &units.rows {
            let (Some(lane), Some(status)) = (
                row.get::<String>("lane"),
                row.get::<String>("status")
                    .and_then(|s| JobStatus::parse(&s)),
            ) else {
                continue;
            };
            let n = row
                .get::<i64>("n")
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or(0);
            *lanes
                .entry(lane)
                .or_default()
                .units
                .entry(status)
                .or_default() += n;
        }
        let members = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT u.lane AS lane, \
                            COUNT(CASE WHEN m.compiled_at IS NULL THEN 1 END) AS pending, \
                            COUNT(CASE WHEN m.compiled_at IS NOT NULL THEN 1 END) AS compiled \
                     FROM source_compile_members m \
                     JOIN source_compile_units u ON u.scope = m.scope AND u.unit_key = m.unit_key \
                     WHERE u.workspace = ? AND u.scope IN ({marks}) GROUP BY u.lane"
                ),
                values,
            ))
            .await?;
        for row in &members.rows {
            let Some(lane) = row.get::<String>("lane") else {
                continue;
            };
            let counts = lanes.entry(lane).or_default();
            let count = |name: &str| {
                row.get::<i64>(name)
                    .and_then(|n| u64::try_from(n).ok())
                    .unwrap_or(0)
            };
            counts.sources_pending = count("pending");
            counts.sources_compiled = count("compiled");
        }
        // Stored and not filed yet: everything after the backlog cursor. The
        // read is bounded by what the pass has not reached, not by the ledger.
        let cursor = self.source_cursor().await?.unwrap_or_default();
        let kinds = vec!["?"; UNIT_KINDS.len()].join(", ");
        let mut values = vec![text(workspace)];
        values.extend(scopes.iter().map(|scope| text(scope)));
        values.push(text(&cursor));
        values.extend(UNIT_KINDS.iter().map(|kind| text(kind)));
        let unfiled = self
            .db
            .query(&Statement::with_values(
                format!(
                    "SELECT kind, COUNT(*) AS n FROM sources \
                     WHERE workspace = ? AND scope IN ({marks}) AND held = 0 AND id > ? \
                       AND kind IN ({kinds}) GROUP BY kind"
                ),
                values,
            ))
            .await?;
        for row in &unfiled.rows {
            let Some(kind) = row.get::<String>("kind") else {
                continue;
            };
            let Some(place) = placement(&kind, "", "", "") else {
                continue;
            };
            let n = row
                .get::<i64>("n")
                .and_then(|n| u64::try_from(n).ok())
                .unwrap_or(0);
            lanes
                .entry(place.lane.to_owned())
                .or_default()
                .sources_pending += n;
        }
        Ok(lanes)
    }
}

/// The project folder a unit's pages go under: a repository's name, a
/// channel's server, or an import's own top folder (else `imports`).
#[must_use]
pub fn unit_label(lane: &str, rel_path: &str, project: Option<&str>) -> String {
    let repo = rel_path
        .strip_prefix("github/")
        .and_then(|rest| rest.split('/').nth(1))
        .filter(|name| !name.is_empty());
    let raw = match lane {
        LANE_GITHUB => repo.or(project).unwrap_or("github"),
        LANE_DISCORD => project.unwrap_or("discord"),
        _ => project
            .or_else(|| rel_path.split_once('/').map(|(top, _)| top))
            .unwrap_or("imports"),
    };
    crate::layout::project_segment(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_item_files_under_its_repository_day() {
        let placed = placement(
            "github_pr",
            "01H",
            "github/acme/widgets/pr/12",
            "2026-10-15T08:00:00Z",
        )
        .expect("a unit");
        assert_eq!(placed.unit_key, "github:acme/widgets:2026-10-15");
        assert_eq!(placed.lane, LANE_GITHUB);
        assert_eq!(placed.unit_day, "2026-10-15");
        assert_eq!(
            unit_label(LANE_GITHUB, "github/acme/widgets/pr/12", None),
            "widgets"
        );
    }

    #[test]
    fn discord_and_imports_are_one_unit_each() {
        let discord = placement(
            "discord",
            "01A",
            "discord/g/c/2026-10-15",
            "2026-10-15T01:00:00Z",
        )
        .expect("a unit");
        assert_eq!(discord.unit_key, "discord:01A");
        let import =
            placement("import", "01B", "notes/a.md", "2026-10-15T01:00:00Z").expect("a unit");
        assert_eq!(import.unit_key, "import:01B");
        assert_eq!(unit_label(LANE_IMPORTS, "notes/a.md", None), "notes");
        assert_eq!(unit_label(LANE_IMPORTS, "a.md", None), "imports");
        assert!(
            placement(
                "agent_log",
                "01C",
                "claude-code session x",
                "2026-10-15T01:00:00Z"
            )
            .is_none()
        );
    }
}
