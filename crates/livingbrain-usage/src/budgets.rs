//! The `usage_budgets` table: a workspace's own budgets and limits, as the
//! "Usage & limits" settings tab saves them.
//!
//! Every limit is an `Option`, and `None` is "not set here": the reader
//! falls through to the deployment's env var and then to the built-in
//! default. This module stores the choices and resolves the one number it
//! owns — the monthly spend limit ([`budget_cents`]); the compiler's and the
//! ask route's numbers are resolved by the crates that own their defaults
//! and their ceilings.
//!
//! A read that fails is an empty [`Budgets`] ([`budgets_or_default`]): a lost
//! read must never stop the brain, and the env var and the default are what
//! the workspace ran on before this table existed.

use cratefield_core::{Config, Database, ModuleConfig, Row, Statement};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

use crate::cost::DEFAULT_BUDGET_CENTS_MONTHLY;
use crate::store::{UsageError, is_workspace_id};

/// A workspace's own budgets and limits. `None` inherits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Budgets {
    /// The coding-agents compiler is paused: no session compiles, by cron or
    /// by "Process now", until it is resumed.
    pub compiler_paused: Option<bool>,
    /// Compile tokens per UTC day.
    pub compiler_daily_tokens: Option<u64>,
    /// Compile tokens per UTC month.
    pub compiler_monthly_tokens: Option<u64>,
    /// One session's tokens, in and out, across its calls.
    pub compiler_job_tokens: Option<u64>,
    /// Sessions one cron run compiles for this workspace.
    pub compiler_batch: Option<u64>,
    /// Model calls one session may take.
    pub compiler_max_chunks: Option<u64>,
    /// Transcript characters per model call.
    pub compiler_chunk_chars: Option<u64>,
    /// The completion ceiling per compile call.
    pub compiler_max_output_tokens: Option<u64>,
    /// The most characters of brief one ask sends.
    pub ask_context_chars: Option<u64>,
    /// The completion ceiling of one answer.
    pub ask_max_output_tokens: Option<u64>,
    /// The monthly spend limit, in US cents.
    pub spend_cents_monthly: Option<u64>,
    /// Triage before a compile: trivial sessions skipped, failing loops
    /// and lookups summarised in one log line, no model call for either.
    pub compiler_triage: Option<bool>,
    /// Compaction of bulky tool output before a compile.
    pub compiler_compaction: Option<bool>,
    /// How readily triage holds a session back: 0 lenient, 1 balanced,
    /// 2 strict.
    pub compiler_triage_strictness: Option<u64>,
}

/// A stored row: the choices, and when they were last saved.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredBudgets {
    pub budgets: Budgets,
    /// RFC 3339, when the row was last saved.
    pub updated_at: Option<String>,
}

/// The columns, in one place, so the read and the write cannot disagree.
const COLUMNS: [&str; 14] = [
    "compiler_paused",
    "compiler_daily_tokens",
    "compiler_monthly_tokens",
    "compiler_job_tokens",
    "compiler_batch",
    "compiler_max_chunks",
    "compiler_chunk_chars",
    "compiler_max_output_tokens",
    "ask_context_chars",
    "ask_max_output_tokens",
    "spend_cents_monthly",
    "compiler_triage",
    "compiler_compaction",
    "compiler_triage_strictness",
];

impl Budgets {
    /// The values in [`COLUMNS`] order, the switches as 0/1.
    fn values(&self) -> [Option<u64>; 14] {
        [
            self.compiler_paused.map(u64::from),
            self.compiler_daily_tokens,
            self.compiler_monthly_tokens,
            self.compiler_job_tokens,
            self.compiler_batch,
            self.compiler_max_chunks,
            self.compiler_chunk_chars,
            self.compiler_max_output_tokens,
            self.ask_context_chars,
            self.ask_max_output_tokens,
            self.spend_cents_monthly,
            self.compiler_triage.map(u64::from),
            self.compiler_compaction.map(u64::from),
            self.compiler_triage_strictness,
        ]
    }

    fn from_row(row: &Row) -> Self {
        let number = |column: &str| row.get::<u64>(column);
        Self {
            compiler_paused: number("compiler_paused").map(|flag| flag != 0),
            compiler_daily_tokens: number("compiler_daily_tokens"),
            compiler_monthly_tokens: number("compiler_monthly_tokens"),
            compiler_job_tokens: number("compiler_job_tokens"),
            compiler_batch: number("compiler_batch"),
            compiler_max_chunks: number("compiler_max_chunks"),
            compiler_chunk_chars: number("compiler_chunk_chars"),
            compiler_max_output_tokens: number("compiler_max_output_tokens"),
            ask_context_chars: number("ask_context_chars"),
            ask_max_output_tokens: number("ask_max_output_tokens"),
            spend_cents_monthly: number("spend_cents_monthly"),
            compiler_triage: number("compiler_triage").map(|flag| flag != 0),
            compiler_compaction: number("compiler_compaction").map(|flag| flag != 0),
            compiler_triage_strictness: number("compiler_triage_strictness"),
        }
    }
}

/// The workspace's stored budgets; a workspace that never saved any has the
/// all-`None` row.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`] and [`UsageError::Store`].
pub async fn budgets(db: &dyn Database, workspace_id: &str) -> Result<StoredBudgets, UsageError> {
    if !is_workspace_id(workspace_id) {
        return Err(UsageError::InvalidWorkspace(workspace_id.to_owned()));
    }
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {}, updated_at FROM usage_budgets WHERE workspace_id = ?",
                COLUMNS.join(", ")
            ),
            vec![text(workspace_id)],
        ))
        .await
        .map_err(UsageError::Store)?;
    Ok(rows
        .first()
        .map_or_else(StoredBudgets::default, |row| StoredBudgets {
            budgets: Budgets::from_row(row),
            updated_at: row.get::<String>("updated_at"),
        }))
}

/// The workspace's budgets, or none at all when the read fails: every
/// reader then runs on the env var and the default, which is what the
/// workspace ran on before it chose anything.
pub async fn budgets_or_default(db: &dyn Database, workspace_id: &str) -> Budgets {
    budgets(db, workspace_id)
        .await
        .map(|stored| stored.budgets)
        .unwrap_or_default()
}

/// Replaces the workspace's budgets with `budgets` — every column, so a
/// `None` clears what was there — and records when. Range checks are the
/// caller's: this stores what it is given.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`] and [`UsageError::Store`].
pub async fn save_budgets(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    budgets: &Budgets,
) -> Result<(), UsageError> {
    if !is_workspace_id(workspace_id) {
        return Err(UsageError::InvalidWorkspace(workspace_id.to_owned()));
    }
    let updated_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let placeholders = vec!["?"; COLUMNS.len()].join(", ");
    let updates = COLUMNS
        .iter()
        .map(|column| format!("{column} = excluded.{column}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut values = vec![text(workspace_id)];
    values.extend(budgets.values().into_iter().map(big));
    values.push(text(&updated_at));
    db.execute(&Statement::with_values(
        format!(
            "INSERT INTO usage_budgets (workspace_id, {}, updated_at) \
             VALUES (?, {placeholders}, ?) \
             ON CONFLICT (workspace_id) DO UPDATE SET {updates}, \
                updated_at = excluded.updated_at",
            COLUMNS.join(", ")
        ),
        values,
    ))
    .await
    .map_err(UsageError::Store)?;
    Ok(())
}

/// The deployment's monthly spend limit: `USAGE_BUDGET_CENTS_MONTHLY`, else
/// [`DEFAULT_BUDGET_CENTS_MONTHLY`]. What a workspace inherits when it has
/// not set its own.
#[must_use]
pub fn deployment_budget_cents(config: &dyn Config) -> u64 {
    ModuleConfig::new("usage", config)
        .get_opt("BUDGET_CENTS_MONTHLY")
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUDGET_CENTS_MONTHLY)
}

/// The effective monthly spend limit, in cents: the workspace's own when it
/// set one, else the deployment's ([`deployment_budget_cents`]). No ceiling:
/// a higher limit spends the workspace's own model key, not ours.
#[must_use]
pub fn budget_cents(config: &dyn Config, budgets: &Budgets) -> u64 {
    budgets
        .spend_cents_monthly
        .unwrap_or_else(|| deployment_budget_cents(config))
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// A nullable number bound as `BIGINT`: `None` is SQL `NULL`.
fn big(value: Option<u64>) -> SeaValue {
    SeaValue::BigInt(value.map(|value| i64::try_from(value).unwrap_or(i64::MAX)))
}
