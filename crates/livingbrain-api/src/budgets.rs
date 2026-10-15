//! The `settings` module: a workspace's budgets and limits as settings
//! people can change, instead of deploy-time env vars.
//!
//! - `GET /v1/settings/budgets` — any member: every limit's **effective**
//!   value, where it comes from (`"workspace"` when the workspace set it,
//!   `"default"` when it inherits the deployment's env var or the built-in
//!   default), what it would be without the workspace's choice
//!   (`inherited`), its allowed range, whether the caller may change it,
//!   and the usage so far against it (compile tokens today and this month,
//!   spend this month).
//! - `PATCH /v1/settings/budgets` — owner or admin only: a partial body in
//!   the same groups the read answers with (`coding_agents`, `ask`,
//!   `spend`); a number sets a limit, `null` clears it back to the
//!   deployment's, a key left out is unchanged. Every value is checked
//!   against its range and a refusal is one 422 naming the setting and the
//!   range. A cookie write must be same-origin; a bearer write needs a
//!   full-access token (a scoped token is never admin).
//!
//! **Precedence**, one rule for every limit: the workspace's own setting,
//! else the env var (`CODING_AGENTS_*`, `ASK_CONTEXT_CHARS_MAX`,
//! `USAGE_BUDGET_CENTS_MONTHLY`), else the built-in default. **Hard
//! ceilings** sit above all three where a Worker's limits need them — the
//! sessions one run compiles ([`BATCH_CEILING`]), the calls and tokens one
//! session may take ([`MAX_CHUNKS_CEILING`], [`JOB_TOKENS_CEILING`]), and
//! the size of one compile call ([`CHUNK_CHARS_CEILING`],
//! [`MAX_OUTPUT_TOKENS_CEILING`]): no setting and no env var lifts a compile
//! past what one invocation can finish. The token budgets and the spend
//! limit have no ceiling — a higher one spends the workspace's own model
//! key.
//!
//! The table is the usage module's (`usage_budgets`): this module reads and
//! writes it through `livingbrain_usage`, which owns the spend limit; the
//! compiler and the ask route resolve their own numbers from the same row
//! ([`compiler_settings`], [`ask_context_ceiling`], [`ask_max_tokens`]).

use std::sync::Arc;

use cratefield_core::axum::Json;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::routing::get;
use cratefield_core::{
    Clock, Config, ConfigError, Database, Migrations, Module, ModuleContext, Port, Problem,
    ProblemDef,
};
use livingbrain_compiler::{
    BATCH_CEILING, CHUNK_CHARS_CEILING, JOB_TOKENS_CEILING, MAX_CHUNKS_CEILING,
    MAX_OUTPUT_TOKENS_CEILING, Overrides, Queue, Settings, Tuning,
};
use livingbrain_models::invoke;
use livingbrain_usage::Budgets;
use serde_json::{Map, Value, json};

use crate::ask::{ASK_CONTEXT_CHARS_MAX, DEFAULT_CONTEXT_CHARS_MAX, MAX_ANSWER_TOKENS};
use crate::coding_agents::{MODEL_ROLE, month_spend_micro_usd};
use crate::port;
use crate::problems::CROSS_SITE_REQUEST;

/// The largest brief one ask may send, in characters, however a workspace
/// sets it: about 250-330k tokens, past which a single question is a bill.
pub const ASK_CONTEXT_CHARS_CEILING: u64 = 1_000_000;
/// The smallest brief a workspace may set.
pub const ASK_CONTEXT_CHARS_FLOOR: u64 = 2_000;
/// The longest answer a workspace may ask for, in tokens.
pub const ASK_MAX_TOKENS_CEILING: u64 = 8_192;
/// The shortest answer a workspace may set, in tokens.
pub const ASK_MAX_TOKENS_FLOOR: u64 = 128;

/// Only an owner or an admin changes a workspace's budgets.
const FORBIDDEN: ProblemDef = ProblemDef {
    slug: "settings/forbidden",
    status: StatusCode::FORBIDDEN,
    title: "Owner or admin access required",
    description: "Only a workspace owner or admin can change its budgets and limits. \
                  Members can read them.",
};

/// A value outside what a setting accepts.
const INVALID: ProblemDef = ProblemDef {
    slug: "settings/invalid-budget",
    status: StatusCode::UNPROCESSABLE_ENTITY,
    title: "A budget or limit was refused",
    description: "One of the values is not a whole number in the setting's range, or names \
                  no setting. Nothing was saved.",
};

/// One setting: where it lives in the body, its range, and how it reads
/// and writes the stored row.
struct Field {
    group: &'static str,
    key: &'static str,
    /// A pause switch (a boolean), not a number.
    switch: bool,
    min: u64,
    max: u64,
    get: fn(&Budgets) -> Option<u64>,
    set: fn(&mut Budgets, Option<u64>),
}

/// The upper bound of a budget with no hard ceiling — far past any real
/// month, and still inside an i64 column.
const UNBOUNDED: u64 = 1_000_000_000_000;

/// Every setting the tab shows, in the order it shows them.
const FIELDS: [Field; 11] = [
    Field {
        group: "coding_agents",
        key: "paused",
        switch: true,
        min: 0,
        max: 1,
        get: |b| b.compiler_paused.map(u64::from),
        set: |b, v| b.compiler_paused = v.map(|v| v != 0),
    },
    Field {
        group: "coding_agents",
        key: "daily_tokens",
        switch: false,
        min: 0,
        max: UNBOUNDED,
        get: |b| b.compiler_daily_tokens,
        set: |b, v| b.compiler_daily_tokens = v,
    },
    Field {
        group: "coding_agents",
        key: "monthly_tokens",
        switch: false,
        min: 0,
        max: UNBOUNDED,
        get: |b| b.compiler_monthly_tokens,
        set: |b, v| b.compiler_monthly_tokens = v,
    },
    Field {
        group: "coding_agents",
        key: "batch",
        switch: false,
        min: 1,
        max: BATCH_CEILING as u64,
        get: |b| b.compiler_batch,
        set: |b, v| b.compiler_batch = v,
    },
    Field {
        group: "coding_agents",
        key: "job_tokens",
        switch: false,
        min: 1_000,
        max: JOB_TOKENS_CEILING,
        get: |b| b.compiler_job_tokens,
        set: |b, v| b.compiler_job_tokens = v,
    },
    Field {
        group: "coding_agents",
        key: "max_chunks",
        switch: false,
        min: 1,
        max: MAX_CHUNKS_CEILING as u64,
        get: |b| b.compiler_max_chunks,
        set: |b, v| b.compiler_max_chunks = v,
    },
    Field {
        group: "coding_agents",
        key: "chunk_chars",
        switch: false,
        min: 1_000,
        max: CHUNK_CHARS_CEILING as u64,
        get: |b| b.compiler_chunk_chars,
        set: |b, v| b.compiler_chunk_chars = v,
    },
    Field {
        group: "coding_agents",
        key: "max_output_tokens",
        switch: false,
        min: 256,
        max: MAX_OUTPUT_TOKENS_CEILING as u64,
        get: |b| b.compiler_max_output_tokens,
        set: |b, v| b.compiler_max_output_tokens = v,
    },
    Field {
        group: "ask",
        key: "context_chars",
        switch: false,
        min: ASK_CONTEXT_CHARS_FLOOR,
        max: ASK_CONTEXT_CHARS_CEILING,
        get: |b| b.ask_context_chars,
        set: |b, v| b.ask_context_chars = v,
    },
    Field {
        group: "ask",
        key: "max_output_tokens",
        switch: false,
        min: ASK_MAX_TOKENS_FLOOR,
        max: ASK_MAX_TOKENS_CEILING,
        get: |b| b.ask_max_output_tokens,
        set: |b, v| b.ask_max_output_tokens = v,
    },
    Field {
        group: "spend",
        key: "monthly_cents",
        switch: false,
        min: 0,
        max: 100_000_000,
        get: |b| b.spend_cents_monthly,
        set: |b, v| b.spend_cents_monthly = v,
    },
];

/// The keys the web app's settings choke point adds beside every change
/// (`app/assets/settings.js`), accepted and ignored here.
const AUDIT_KEYS: [&str; 3] = ["section", "key", "value"];

/// The compiler's view of a workspace's stored choices.
#[must_use]
pub fn overrides(budgets: &Budgets) -> Overrides {
    Overrides {
        paused: budgets.compiler_paused,
        batch: budgets.compiler_batch,
        chunk_chars: budgets.compiler_chunk_chars,
        max_chunks: budgets.compiler_max_chunks,
        max_output_tokens: budgets.compiler_max_output_tokens,
        job_tokens: budgets.compiler_job_tokens,
        daily_tokens: budgets.compiler_daily_tokens,
        monthly_tokens: budgets.compiler_monthly_tokens,
    }
}

/// The settings a workspace compiles under: the env vars (or defaults), with
/// its own choices on top, clamped to the hard ceilings.
#[must_use]
pub fn compiler_settings(config: &dyn Config, budgets: &Budgets) -> Settings {
    Settings::from_config(config).with_overrides(&overrides(budgets))
}

/// The deployment's ask ceiling: `ASK_CONTEXT_CHARS_MAX`, or its default.
fn deployment_context_ceiling(config: &dyn Config) -> u64 {
    config
        .get(ASK_CONTEXT_CHARS_MAX)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|chars| *chars > 0)
        .unwrap_or(DEFAULT_CONTEXT_CHARS_MAX as u64)
}

/// The most brief one ask sends in this workspace: its own setting, else
/// `ASK_CONTEXT_CHARS_MAX`, else the default — the ceiling the model's own
/// window is then sized under (`context_budget`).
#[must_use]
pub fn ask_context_ceiling(config: &dyn Config, budgets: &Budgets) -> usize {
    let chars = budgets.ask_context_chars.map_or_else(
        || deployment_context_ceiling(config),
        |own| own.clamp(ASK_CONTEXT_CHARS_FLOOR, ASK_CONTEXT_CHARS_CEILING),
    );
    usize::try_from(chars).unwrap_or(usize::MAX)
}

/// The completion ceiling of one answer in this workspace: its own setting,
/// else [`MAX_ANSWER_TOKENS`].
#[must_use]
pub fn ask_max_tokens(budgets: &Budgets) -> u32 {
    budgets
        .ask_max_output_tokens
        .map_or(MAX_ANSWER_TOKENS, |own| {
            u32::try_from(own.clamp(ASK_MAX_TOKENS_FLOOR, ASK_MAX_TOKENS_CEILING))
                .unwrap_or(MAX_ANSWER_TOKENS)
        })
}

/// The compiler's [`Tuning`] over the stored row.
pub(crate) struct StoredTuning {
    pub(crate) db: Arc<dyn Database>,
}

#[async_trait::async_trait]
impl Tuning for StoredTuning {
    async fn overrides(&self, workspace: &str) -> Overrides {
        overrides(&livingbrain_usage::budgets_or_default(&*self.db, workspace).await)
    }
}

/// The `settings` module, mounted at `/v1/settings`.
#[derive(Debug, Default)]
pub struct BudgetSettings;

impl BudgetSettings {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Module for BudgetSettings {
    fn name(&self) -> &'static str {
        "settings"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The database holds the row and the ledgers; the clock stamps a save
    /// and fixes today; the signer verifies the session cookie.
    fn requires(&self) -> &'static [Port] {
        &[Port::Db, Port::Clock, Port::Signer]
    }

    /// The table is the usage module's, which declares it and its personal
    /// data; this module owns no table.
    fn migrations(&self) -> Migrations {
        Migrations::EMPTY
    }

    fn validate_config(&self, _cfg: &dyn Config) -> Result<(), ConfigError> {
        Ok(())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        cratefield_core::axum::Router::new()
            .route("/budgets", get(read).patch(update))
            .with_state(Arc::new(ctx))
    }
}

/// The caller, by bearer or by session cookie; a cookie write must be
/// same-origin.
async fn caller(
    ctx: &ModuleContext,
    headers: &HeaderMap,
    write: bool,
) -> Result<livingbrain_tokens::Member, Problem> {
    if write && cratefield_core::bearer_token(headers).is_none() {
        livingbrain_tokens::same_origin(headers)
            .map_err(|refused| Problem::new(&CROSS_SITE_REQUEST).with_detail(refused.detail))?;
    }
    Ok(livingbrain_tokens::authenticate(&ctx.ports, headers).await?)
}

/// `GET /budgets`.
async fn read(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let member = caller(&ctx, &headers, false).await?;
    view(&ctx, &member).await.map(Json)
}

/// `PATCH /budgets`.
async fn update(
    State(ctx): State<Arc<ModuleContext>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, Problem> {
    let member = caller(&ctx, &headers, true).await?;
    if !member.is_admin {
        return Err(Problem::new(&FORBIDDEN));
    }
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let patch: Value = serde_json::from_slice(&body)
        .map_err(|_| Problem::validation_failed("the body is not JSON"))?;
    let stored = livingbrain_usage::budgets(&*db, &member.workspace_id)
        .await
        .map_err(|_| Problem::internal())?;
    let budgets = apply(stored.budgets, &patch)
        .map_err(|detail| Problem::new(&INVALID).with_detail(detail))?;
    livingbrain_usage::save_budgets(&*db, clock.now(), &member.workspace_id, &budgets)
        .await
        .map_err(|_| Problem::internal())?;
    view(&ctx, &member).await.map(Json)
}

/// `patch` applied over `budgets`, or the one sentence that says why not.
fn apply(mut budgets: Budgets, patch: &Value) -> Result<Budgets, String> {
    let Some(groups) = patch.as_object() else {
        return Err("the body must be a JSON object of setting groups".to_owned());
    };
    for (group, entries) in groups {
        if AUDIT_KEYS.contains(&group.as_str()) {
            continue;
        }
        if !FIELDS.iter().any(|field| field.group == group) {
            return Err(format!(
                "`{group}` is not a settings group (coding_agents, ask, spend)"
            ));
        }
        let Some(entries) = entries.as_object() else {
            return Err(format!("`{group}` must be an object of settings"));
        };
        for (key, value) in entries {
            let Some(field) = FIELDS
                .iter()
                .find(|field| field.group == group && field.key == key)
            else {
                return Err(format!("`{group}.{key}` is not a setting"));
            };
            let parsed = parse(field, value)?;
            (field.set)(&mut budgets, parsed);
        }
    }
    Ok(budgets)
}

/// One value: `null` clears, a switch takes `true`/`false`, a number must be
/// a whole number in the field's range.
fn parse(field: &Field, value: &Value) -> Result<Option<u64>, String> {
    let name = format!("{}.{}", field.group, field.key);
    if value.is_null() {
        return Ok(None);
    }
    if field.switch {
        return value
            .as_bool()
            .map(|on| Some(u64::from(on)))
            .ok_or_else(|| format!("`{name}` must be true, false or null"));
    }
    let Some(number) = value.as_u64() else {
        return Err(format!(
            "`{name}` must be a whole number from {} to {}, or null to use the default",
            field.min, field.max
        ));
    };
    if number < field.min || number > field.max {
        return Err(format!(
            "`{name}` must be from {} to {}; {number} is out of range",
            field.min, field.max
        ));
    }
    Ok(Some(number))
}

/// The answer both routes give: every setting, the usage against it, and
/// whether the caller may change it.
async fn view(ctx: &ModuleContext, member: &livingbrain_tokens::Member) -> Result<Value, Problem> {
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let config = &*ctx.config;
    let workspace = &member.workspace_id;
    let stored = livingbrain_usage::budgets(&*db, workspace)
        .await
        .map_err(|_| Problem::internal())?;
    let own = &stored.budgets;
    let none = Budgets::default();

    let effective = compiler_settings(config, own);
    let inherited = compiler_settings(config, &none);
    let compiler_value = |key: &str, settings: &Settings| -> u64 {
        match key {
            "paused" => u64::from(settings.paused),
            "daily_tokens" => settings.daily_tokens,
            "monthly_tokens" => settings.monthly_tokens,
            "batch" => settings.batch as u64,
            "job_tokens" => settings.job_tokens,
            "max_chunks" => settings.max_chunks as u64,
            "chunk_chars" => settings.chunk_chars as u64,
            _ => u64::from(settings.max_output_tokens),
        }
    };
    let value_of = |field: &Field, budgets: &Budgets, settings: &Settings| -> u64 {
        match field.group {
            "coding_agents" => compiler_value(field.key, settings),
            "ask" if field.key == "context_chars" => ask_context_ceiling(config, budgets) as u64,
            "ask" => u64::from(ask_max_tokens(budgets)),
            _ => livingbrain_usage::budget_cents(config, budgets),
        }
    };

    let mut groups: Map<String, Value> = Map::new();
    for field in &FIELDS {
        let set = (field.get)(own).is_some();
        let value = value_of(field, own, &effective);
        let fallback = value_of(field, &none, &inherited);
        let (value, fallback) = if field.switch {
            (json!(value != 0), json!(fallback != 0))
        } else {
            (json!(value), json!(fallback))
        };
        let entry = json!({
            "value": value,
            "source": if set { "workspace" } else { "default" },
            "inherited": fallback,
            "min": if field.switch { Value::Null } else { json!(field.min) },
            "max": if field.switch { Value::Null } else { json!(field.max) },
        });
        groups
            .entry(field.group.to_owned())
            .or_insert_with(|| Value::Object(Map::new()))
            .as_object_mut()
            .map(|group| group.insert(field.key.to_owned(), entry));
    }

    // Usage so far: the compile tokens against the daily and monthly
    // budgets, and the month's attributed spend against the limit.
    let now = clock.now();
    let today = livingbrain_compiler::queue::utc_day(now);
    let first = format!("{}-01", &today[..7]);
    let queue = Queue::new(db.clone());
    let spent_today = queue
        .spent(workspace, &today, &today)
        .await
        .map_err(|_| Problem::internal())?;
    let spent_month = queue
        .spent(workspace, &first, &today)
        .await
        .map_err(|_| Problem::internal())?;
    let serving = invoke::serving(&*db, workspace, MODEL_ROLE)
        .await
        .map_err(|_| Problem::internal())?;
    let spend =
        month_spend_micro_usd(&*db, &*clock as &dyn Clock, workspace, serving.as_ref()).await;

    Ok(json!({
        "workspace_id": workspace,
        "role": member.role.as_str(),
        "editable": member.is_admin,
        "settings": groups,
        "usage": {
            "compile_tokens_today": spent_today,
            "compile_tokens_month": spent_month,
            "spend_month_usd_micros": spend.map(|(micros, _)| micros),
            "spend_model_priced": spend.is_some_and(|(_, priced)| priced),
        },
        "ceilings": {
            "batch": BATCH_CEILING,
            "job_tokens": JOB_TOKENS_CEILING,
            "max_chunks": MAX_CHUNKS_CEILING,
            "chunk_chars": CHUNK_CHARS_CEILING,
            "max_output_tokens": MAX_OUTPUT_TOKENS_CEILING,
        },
        "updated_at": stored.updated_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::{EmptyConfig, MapConfig};

    /// Setting > env var > default, for each kind of number.
    #[test]
    fn a_workspace_setting_wins_over_the_env_var_which_wins_over_the_default() {
        let env = MapConfig::from_pairs([
            ("CODING_AGENTS_DAILY_TOKENS", "3000000"),
            ("CODING_AGENTS_BATCH", "3"),
            ("ASK_CONTEXT_CHARS_MAX", "50000"),
        ]);
        let none = Budgets::default();
        // Default, then env.
        assert_eq!(compiler_settings(&EmptyConfig, &none).daily_tokens, 400_000);
        assert_eq!(compiler_settings(&env, &none).daily_tokens, 3_000_000);
        assert_eq!(compiler_settings(&env, &none).batch, 3);
        assert_eq!(ask_context_ceiling(&EmptyConfig, &none), 200_000);
        assert_eq!(ask_context_ceiling(&env, &none), 50_000);
        assert_eq!(ask_max_tokens(&none), MAX_ANSWER_TOKENS);
        // The workspace's own.
        let own = Budgets {
            compiler_daily_tokens: Some(9_000_000),
            compiler_batch: Some(5),
            ask_context_chars: Some(120_000),
            ask_max_output_tokens: Some(2_048),
            ..Budgets::default()
        };
        assert_eq!(compiler_settings(&env, &own).daily_tokens, 9_000_000);
        assert_eq!(compiler_settings(&env, &own).batch, 5);
        assert_eq!(ask_context_ceiling(&env, &own), 120_000);
        assert_eq!(ask_max_tokens(&own), 2_048);
    }

    /// The hard ceilings hold whatever a row or an env var says.
    #[test]
    fn the_hard_ceilings_hold_against_a_row_and_an_env_var() {
        let env = MapConfig::from_pairs([
            ("CODING_AGENTS_BATCH", "500"),
            ("CODING_AGENTS_JOB_TOKENS", "999999999"),
        ]);
        let settings = compiler_settings(&env, &Budgets::default());
        assert_eq!(settings.batch, BATCH_CEILING);
        assert_eq!(settings.job_tokens, JOB_TOKENS_CEILING);
        // A row written past the API's checks (by hand, or by an older
        // version) is clamped all the same.
        let wild = Budgets {
            compiler_batch: Some(1_000),
            compiler_job_tokens: Some(u64::MAX),
            compiler_max_chunks: Some(0),
            ask_context_chars: Some(u64::MAX),
            ask_max_output_tokens: Some(1),
            ..Budgets::default()
        };
        let settings = compiler_settings(&EmptyConfig, &wild);
        assert_eq!(settings.batch, BATCH_CEILING);
        assert_eq!(settings.job_tokens, JOB_TOKENS_CEILING);
        assert_eq!(settings.max_chunks, 1);
        assert_eq!(
            ask_context_ceiling(&EmptyConfig, &wild),
            ASK_CONTEXT_CHARS_CEILING as usize
        );
        assert_eq!(ask_max_tokens(&wild), ASK_MAX_TOKENS_FLOOR as u32);
    }

    /// A patch sets, clears and leaves alone; a bad one names the setting.
    #[test]
    fn a_patch_sets_clears_and_refuses_with_the_setting_named() {
        let start = Budgets {
            compiler_batch: Some(2),
            spend_cents_monthly: Some(900),
            ..Budgets::default()
        };
        let patched = apply(
            start.clone(),
            &json!({
                "section": "budgets", "key": "all", "value": {},
                "coding_agents": {"paused": true, "daily_tokens": 3_000_000, "batch": null},
            }),
        )
        .expect("a valid patch");
        assert_eq!(patched.compiler_paused, Some(true));
        assert_eq!(patched.compiler_daily_tokens, Some(3_000_000));
        assert_eq!(patched.compiler_batch, None, "null clears");
        assert_eq!(
            patched.spend_cents_monthly,
            Some(900),
            "absent is unchanged"
        );

        let refused = |patch: Value| apply(start.clone(), &patch).expect_err("refused");
        assert!(refused(json!({"coding_agents": {"batch": 21}})).contains("coding_agents.batch"));
        assert!(refused(json!({"coding_agents": {"batch": 0}})).contains("from 1 to 20"));
        assert!(refused(json!({"coding_agents": {"job_tokens": 2_000_000}})).contains("1000000"));
        assert!(refused(json!({"coding_agents": {"daily_tokens": -1}})).contains("whole number"));
        assert!(refused(json!({"coding_agents": {"daily_tokens": 1.5}})).contains("whole number"));
        assert!(refused(json!({"coding_agents": {"paused": 1}})).contains("true, false"));
        assert!(refused(json!({"coding_agents": {"nope": 1}})).contains("not a setting"));
        assert!(refused(json!({"billing": {}})).contains("not a settings group"));
        assert!(refused(json!([1])).contains("JSON object"));
    }
}
