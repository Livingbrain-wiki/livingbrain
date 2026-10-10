//! `livingbrain-usage`: the cost ledger — what a hosted workspace's usage
//! cost us today, this month, and against its budget.
//!
//! The table is one row per workspace per UTC day
//! ([`usage_daily`](crate::store)): model requests and tokens, D1 queries,
//! emails sent. The rows are written by atomic increments
//! ([`record_model`], [`record_email`], [`record_d1`]) so concurrent
//! callers merge into the same day instead of overwriting it, and read by
//! one admin endpoint, `GET /v1/usage/daily?days=14`, which adds the cost
//! math ([`crate::cost`]) on top: micro-USD totals, month to date, and
//! where the month stands against the workspace's budget.
//!
//! # What is attributed today
//!
//! - **Model tokens.** Every call made through a connected model — the
//!   connect-time capability probe is the first caller — records its
//!   prompt and completion tokens and one request.
//! - **Email sends.** One row increment per send, priced at
//!   [`EMAIL_MICRO_USD_PER_SEND`]: zero while pilot volume sits inside the
//!   mail provider's free tier.
//!
//! # What is not attributed (yet)
//!
//! **D1 queries.** During the single-workspace pilot, D1 usage is
//! account-level — Cloudflare's dashboard counts queries per database, not
//! per workspace or per query, so there is nothing honest to attribute.
//! The `d1_queries` column is ready for the day query-level attribution
//! lands; until then it reads zero and contributes nothing to the budget,
//! which is a statement about what we can measure, not about what D1
//! costs.
//!
//! # What the budget means
//!
//! `USAGE_BUDGET_CENTS_MONTHLY` (default [`DEFAULT_BUDGET_CENTS_MONTHLY`]
//! = 300 cents) is the **infra budget** a hosted plan's margin leaves:
//! pricing v3 charges $9/mo for Teams hosted where the customer brings
//! their own LLM, so roughly a third of the fee is what the venture's own
//! infrastructure (D1, R2, Worker requests, the mail tier) may cost —
//! about $3.00 at a two-thirds margin. Crew hosted at $15/mo carries $5 of
//! DeepSeek credit a month, so a workspace on Crew pricing model tokens
//! against credit and the rest against the same infra share. The endpoint
//! answers `"over"` once attributed month-to-date cost is strictly past
//! the budget — the pause-the-workspace decision is someone else's; this
//! number is the fact that decision would act on.
//!
//! `model_usd_micros` in the response is `null` unless the workspace has a
//! connected **main**-role model whose `(provider, model)` pair is in the
//! price sheet — a workspace on an unpriced model is *unknown cost*, not
//! zero cost. Deciding that reads the models module's `model_connections`
//! table with one read-only SELECT; both tables live in the same D1
//! database, and the read crosses module lines on purpose: usage owns the
//! bill, models owns the connection, and neither a price-sheet dependency
//! on the models crate nor a duplicated connections table keeps that
//! honest. The read never writes, so the models module's data stays its
//! own.
//!
//! The budget is per workspace per calendar month (UTC), and the month it
//! reads is not the served window: the endpoint fetches the ledger twice —
//! once over the caller's `days=`, once over the month's first day through
//! today (bounded at 31 rows) — so the month-to-date figure and the
//! "over" verdict come from the month-wide [`DayRow`]s, even when the
//! `days` array in the same response covers less of the month. One table,
//! two windows; there is no second source of truth for a number that says
//! "over".

#![forbid(unsafe_code)]

mod cost;
mod store;

pub use cost::{
    BudgetStatus, DEFAULT_BUDGET_CENTS_MONTHLY, EMAIL_MICRO_USD_PER_SEND, MICRO_USD_PER_USD,
    email_cost_micro_usd, model_cost_micro_usd, month_to_date, status,
};
pub use store::{
    DayRow, Totals, UsageError, days, is_day, is_workspace_id, record_d1, record_email,
    record_model, utc_day,
};

use std::collections::HashMap;
use std::sync::Arc;

use cratefield_core::axum::extract::{Query, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::axum::routing::get;
use cratefield_core::axum::{self, Json};
use cratefield_core::{
    Config, ConfigError, Database, DbError, Migrations, Module, ModuleConfig, ModuleContext,
    PersonalDataSet, Port, Problem, ProblemDef, SqlMigration, Statement,
};
use livingbrain_tokens::authenticate;
use serde::Serialize;

/// The config key suffix for the monthly infra budget, read through
/// `ModuleConfig::new("usage", ..)`, so the env var is
/// `USAGE_BUDGET_CENTS_MONTHLY` — a whole number of US cents.
const BUDGET_CENTS_MONTHLY: &str = "BUDGET_CENTS_MONTHLY";

/// The window the endpoint answers with when the query names none.
const DEFAULT_DAYS: u32 = 14;

/// The widest window the endpoint answers with, in days. One calendar
/// month is the granularity the budget lives at; beyond that a caller
/// wants an export, not a JSON array that grows with the ledger.
const MAX_DAYS: u32 = 31;

/// Only a workspace admin or owner can read the bill.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "usage/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Admin or owner access required",
    description: "Only a workspace admin or owner can read the usage ledger.",
};

/// The router state: the module's ports, with the config-derived budget
/// resolved per request.
struct ModuleState {
    ctx: ModuleContext,
}

/// The usage module.
#[derive(Debug, Default)]
pub struct Usage;

impl Usage {
    /// A new usage module. The budget comes from config at request time,
    /// not the builder.
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for Usage {
    fn name(&self) -> &'static str {
        "usage"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The database holds the ledger; the clock stamps each day's
    /// `updated_at` and fixes the windows the endpoint reads; the signer
    /// verifies the session cookie `authenticate` falls back to when a
    /// request carries no bearer token. Nothing else — this module calls
    /// no upstream and mints no ids (a day row's key is
    /// `(workspace_id, day)`).
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock, Port::Signer]
    }

    fn tables(&self) -> &'static [&'static str] {
        &["usage_daily"]
    }

    /// A ledger row names a workspace and a day, and no column names a
    /// person: the counts are the workspace's, pooled. The members behind
    /// the activity are the `workspace_members` rows the workspaces module
    /// already declares.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[PersonalDataSet::none(
            "usage_daily",
            "the row is one workspace's pooled counts for one UTC day — nothing in \
             it identifies or describes a person",
        )];
        SETS
    }

    fn migrations(&self) -> Migrations {
        /// The one migration: `usage_daily`, in the portable SQL subset.
        const MIGRATION_INIT: SqlMigration = SqlMigration::new(
            "0001",
            "init",
            include_str!("../migrations/sqlite/0001_init.sql"),
        );
        /// The Postgres form: the same schema with BIGINT counters — see
        /// the file's header for why the counters diverge from the sqlite
        /// file's INTEGER.
        const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
            "0001",
            "init",
            include_str!("../migrations/postgres/0001_init.sql"),
        );

        const MIGRATIONS: [SqlMigration; 1] = [MIGRATION_INIT];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 1] = [MIGRATION_INIT_POSTGRES];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// `USAGE_BUDGET_CENTS_MONTHLY` is optional; when it is set it must be
    /// a whole number of cents. A deployment that never sets it runs at
    /// [`DEFAULT_BUDGET_CENTS_MONTHLY`], so nothing here demands
    /// configuration the pilot does not need.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        let module = ModuleConfig::new("usage", cfg);
        let Some(raw) = module.get_opt(BUDGET_CENTS_MONTHLY) else {
            return Ok(());
        };
        if raw.trim().parse::<u64>().is_ok() {
            return Ok(());
        }
        let mut errors = ConfigError::new();
        errors.push(format!(
            "usage: {} must be a whole number of cents, got {raw:?}",
            module.key(BUDGET_CENTS_MONTHLY)
        ));
        Err(errors)
    }

    fn router(&self, ctx: ModuleContext) -> axum::Router {
        router(ctx)
    }
}

/// The module's routes, mounted at `/v1/usage`.
pub(crate) fn router(ctx: ModuleContext) -> axum::Router {
    let state = Arc::new(ModuleState { ctx });
    axum::Router::new()
        .route("/daily", get(daily))
        .with_state(state)
}

/// A port the module declared and the runtime did not supply.
fn port<T: ?Sized>(slot: Option<Arc<T>>) -> Result<Arc<T>, Problem> {
    slot.ok_or_else(Problem::internal)
}

/// A failed statement. The adapter's message never reaches the body.
fn database(_error: DbError) -> Problem {
    Problem::internal()
}

/// A refused or unreadable ledger read. Same answer as [`database`]: the
/// caller learns the read failed and nothing more.
fn ledger(_error: store::UsageError) -> Problem {
    Problem::internal()
}

/// The monthly budget in cents, from config; unset or unparsable falls
/// back to [`DEFAULT_BUDGET_CENTS_MONTHLY`]. The unparsable case is
/// refused at boot by `validate_config` — here it falls back rather than
/// turn every read into a 500 for a number the read did not break.
fn budget_cents(cfg: &dyn Config) -> u64 {
    ModuleConfig::new("usage", cfg)
        .get_opt(BUDGET_CENTS_MONTHLY)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUDGET_CENTS_MONTHLY)
}

/// `GET /daily?days=14` — the caller's workspace's ledger for the last
/// `days` UTC days (clamped to 1..=31, default 14), its totals, the
/// month-to-date attributed cost, and where that stands against the
/// budget. Admin or owner only; 401 when nobody is calling at all.
async fn daily(
    State(state): State<Arc<ModuleState>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, Problem> {
    let db = port(state.ctx.ports.db.clone())?;
    let clock = port(state.ctx.ports.clock.clone())?;

    let c = authenticate(&state.ctx.ports, &headers).await?;
    if !c.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let window = parse_days(&query)?;
    let now = clock.now();

    // The window ends today and reaches back `window - 1` days, both as
    // UTC day keys — the same key format the store writes.
    let today = store::utc_day(now);
    let first_day = store::utc_day(now - time::Duration::days(i64::from(window - 1)));
    let rows = store::days(&*db, &c.workspace_id, &first_day, &today)
        .await
        .map_err(ledger)?;
    let totals = store::Totals::of(&rows);

    // The budget's window is the calendar month, which can reach back
    // further than the caller's: a second, month-wide read (bounded at 31
    // rows) feeds month-to-date and the verdict, so spend from before the
    // served window still counts.
    let month_date = now.to_offset(time::UtcOffset::UTC).date();
    let month_first_day = format!(
        "{:04}-{:02}-01",
        month_date.year(),
        u8::from(month_date.month())
    );
    let month_rows = store::days(&*db, &c.workspace_id, &month_first_day, &today)
        .await
        .map_err(ledger)?;

    // Deliberate read-only cross-module read (see the module docs): the
    // main connection's (provider, model) pair decides whether this
    // workspace's model cost is a number or an unknown.
    let main = main_model(&*db, &c.workspace_id).await.map_err(database)?;
    let model_usd_micros = main.as_ref().and_then(|(provider, model)| {
        cost::model_cost_micro_usd(
            provider,
            model,
            totals.prompt_tokens,
            totals.completion_tokens,
        )
    });

    let mtd = cost::month_to_date(&month_rows, now);
    let mtd_model_micros = main
        .as_ref()
        .map(|(provider, model)| {
            cost::model_cost_micro_usd(provider, model, mtd.prompt_tokens, mtd.completion_tokens)
                .unwrap_or(0)
        })
        .unwrap_or(0);
    let month_to_date_usd_micros = mtd_model_micros + cost::email_cost_micro_usd(mtd.emails_sent);

    let budget = budget_cents(&*state.ctx.config);
    Ok(Json(DailyView {
        workspace_id: c.workspace_id,
        days: rows.iter().map(DayView::of).collect(),
        totals: TotalsView::of(&totals, model_usd_micros),
        month_to_date_usd_micros,
        budget_cents: budget,
        status: cost::status(month_to_date_usd_micros, budget).as_str(),
    })
    .into_response())
}

/// The `days` query parameter: absent is [`DEFAULT_DAYS`], garbage is a
/// validation failure, and an out-of-range number is clamped into
/// 1..=[`MAX_DAYS`] — a caller asking for 500 days gets the widest window
/// there is, not an error about a number it could not have meant.
fn parse_days(query: &HashMap<String, String>) -> Result<u32, Problem> {
    match query.get("days") {
        None => Ok(DEFAULT_DAYS),
        Some(raw) => {
            let days = raw.trim().parse::<u32>().map_err(|_| {
                Problem::validation_failed(format!("days must be a whole number, got {raw:?}"))
            })?;
            Ok(days.clamp(1, MAX_DAYS))
        }
    }
}

/// The workspace's main-role model connection, as the `(provider, model)`
/// pair the price sheet is keyed on, or `None` when no main role is
/// connected.
///
/// This is the one deliberate cross-module read (see the module docs): a
/// single SELECT against `model_connections`, the models module's table in
/// the same D1 database. Read-only, workspace-scoped, and named in one
/// place so the day it becomes a port is a one-place change.
async fn main_model(
    db: &dyn Database,
    workspace_id: &str,
) -> Result<Option<(String, String)>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT provider, model FROM model_connections WHERE workspace_id = ? AND role = ?",
            vec![text(workspace_id), text(MAIN_ROLE)],
        ))
        .await?;
    Ok(rows
        .first()
        .and_then(|row| Some((row.get::<String>("provider")?, row.get::<String>("model")?))))
}

/// The role the price sheet reads: the model a workspace actually works
/// through.
const MAIN_ROLE: &str = "main";

fn text(value: &str) -> sea_query::Value {
    sea_query::Value::String(Some(Box::new(value.to_owned())))
}

/// One day of the ledger, as the response's `days` array holds it: the
/// counts and nothing else — pricing happens once, in `totals`.
#[derive(Debug, Serialize)]
struct DayView {
    day: String,
    model_requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    d1_queries: u64,
    emails_sent: u64,
}

impl DayView {
    fn of(row: &store::DayRow) -> Self {
        Self {
            day: row.day.clone(),
            model_requests: row.model_requests,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            d1_queries: row.d1_queries,
            emails_sent: row.emails_sent,
        }
    }
}

/// The window's summed counts, with the cost the price sheet could put on
/// them. `model_usd_micros` is `null` when the workspace's model is not
/// priced — unknown, not free.
#[derive(Debug, Serialize)]
struct TotalsView {
    model_requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    d1_queries: u64,
    emails_sent: u64,
    model_usd_micros: Option<u64>,
    email_usd_micros: u64,
}

impl TotalsView {
    fn of(totals: &store::Totals, model_usd_micros: Option<u64>) -> Self {
        Self {
            model_requests: totals.model_requests,
            prompt_tokens: totals.prompt_tokens,
            completion_tokens: totals.completion_tokens,
            d1_queries: totals.d1_queries,
            emails_sent: totals.emails_sent,
            model_usd_micros,
            email_usd_micros: cost::email_cost_micro_usd(totals.emails_sent),
        }
    }
}

/// The response: the window, its totals, the month's attributed cost, and
/// the budget verdict.
#[derive(Debug, Serialize)]
struct DailyView {
    workspace_id: String,
    days: Vec<DayView>,
    totals: TotalsView,
    month_to_date_usd_micros: u64,
    budget_cents: u64,
    status: &'static str,
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BUDGET_CENTS_MONTHLY, budget_cents};
    use cratefield_core::{EmptyConfig, MapConfig, Module};

    /// The budget default: 300 cents, the $3.00 infra share of the $9/mo
    /// Teams hosted plan (pricing v3).
    #[test]
    fn the_default_budget_is_three_dollars() {
        assert_eq!(DEFAULT_BUDGET_CENTS_MONTHLY, 300);
        assert_eq!(budget_cents(&EmptyConfig), 300);
    }

    /// A set budget parses; a set-but-unparsable one falls back at read
    /// time (and is refused at boot by `validate_config`).
    #[test]
    fn the_budget_reads_the_configured_cents() {
        let set = MapConfig::from_pairs([("USAGE_BUDGET_CENTS_MONTHLY", "450")]);
        assert_eq!(budget_cents(&set), 450);
        let broken = MapConfig::from_pairs([("USAGE_BUDGET_CENTS_MONTHLY", "$4.50")]);
        assert_eq!(budget_cents(&broken), 300);
    }

    /// A malformed budget is a boot error naming the full env key.
    #[test]
    fn a_malformed_budget_is_refused_at_boot() {
        let broken = MapConfig::from_pairs([("USAGE_BUDGET_CENTS_MONTHLY", "lots")]);
        let error = super::Usage.validate_config(&broken).expect_err("refused");
        assert!(
            error.to_string().contains("USAGE_BUDGET_CENTS_MONTHLY"),
            "{error}"
        );
        let unset = MapConfig::from_pairs([("SOMETHING_ELSE", "1")]);
        super::Usage.validate_config(&unset).expect("unset is fine");
    }
}
