//! The local runner: model calls handed to a member's own device.
//!
//! A consumer subscription — Claude Pro/Max through Claude Code, ChatGPT
//! Plus/Pro through Codex — may only be used inside its vendor's own client,
//! so its credential must never reach this server. The subscription path is
//! therefore a **runner** on the member's machine: their `claude -p` or
//! `codex exec` makes the call, and this module only queues the prompt and
//! takes the answer back.
//!
//! - **Who.** A runner is per member ([`set_runner`]): "jobs for me run on my
//!   device". It applies to jobs that member *owns* — the compile of a
//!   session they synced, a question they asked — and wins over the
//!   workspace's connection for them. Nobody else's prompt ever reaches it.
//! - **The job** ([`dispatch`]) is the same redacted prompt an API model would
//!   get, sealed under `MODEL_KEYS_SECRET` (AAD = workspace + job id) while it
//!   waits, and wiped as soon as the job is consumed, failed or expired.
//! - **Claims** ([`claim_next`]) are the owner's only, compare-and-set, with a
//!   lease: a runner that died mid-job leaves a job another poll can take once
//!   the lease lapses, up to [`MAX_ATTEMPTS`].
//! - **Answers** ([`complete`]) are stored for the caller, which finds its
//!   own call again by `(kind, ref_key, prompt hash)` and validates it there:
//!   only the caller knows what a valid answer to its own prompt looks like.
//! - **Usage** is the tokens the runner reported, recorded on the job row; it
//!   costs the brain nothing, so it is not priced into the workspace's ledger.

use cratefield_core::{Clock, Config, Database, DbError, HttpClient, IdGen, Row, Statement};
use sea_query::Value;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};

use crate::crypto;
use crate::invoke::{self, ChatReply, ChatRequest, InvokeError};

/// The one connection kind a member's own row may have.
pub const LOCAL_RUNNER: &str = "local_runner";
/// The CLIs a runner drives.
pub const AGENTS: &[&str] = &["claude", "codex"];
/// How a runner treats the CLI's own settings: `isolated` ignores them,
/// `user` loads them (Claude Code pointed at another Anthropic-compatible
/// endpoint, such as a GLM Coding Plan, lives in the user's settings).
pub const SETTINGS: &[&str] = &["isolated", "user"];
/// How long one claim holds a job.
pub const LEASE: Duration = Duration::minutes(10);
/// How many claims a job gets before it stays failed.
pub const MAX_ATTEMPTS: i64 = 3;
/// How long a job waits for a runner before it expires.
pub const TTL: Duration = Duration::hours(24);
/// The longest answer a runner may hand back.
pub const MAX_RESULT_BYTES: usize = 512 * 1024;
/// The reasons a runner may give for a failure: a fixed vocabulary, so no
/// CLI output can be smuggled into a status column.
pub const FAIL_REASONS: &[&str] = &[
    "timeout",
    "cli-missing",
    "not-logged-in",
    "cli-error",
    "bad-output",
    "cancelled",
];

/// A member's runner for one role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub role: String,
    pub kind: String,
    pub agent: String,
    pub model: String,
    pub settings: String,
    pub updated_at: String,
    pub last_seen_at: Option<String>,
}

/// What a caller is asking on behalf of: the member who owns the work, what
/// kind of work it is, and the caller's own handle for it.
#[derive(Debug, Clone, Copy)]
pub struct JobCall<'a> {
    /// The member whose work this is (a session's syncer, an asker).
    pub owner: &'a str,
    /// `compile` or `ask`: how [`complete`]'s caller validates the answer.
    pub kind: &'a str,
    /// The caller's handle (a compile job and chunk, an ask's question).
    pub ref_key: &'a str,
}

/// How a call went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dispatch {
    /// An answer: from the workspace's API model, or a runner's that had
    /// already arrived.
    Reply(ChatReply),
    /// Handed to the owner's runner; the answer comes back later.
    Pending { job_id: String },
}

/// A job as the API shows it: never its prompt or answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    pub id: String,
    pub owner: String,
    pub role: String,
    pub kind: String,
    pub ref_key: String,
    pub agent: String,
    pub model: String,
    pub status: String,
    pub attempts: i64,
    pub error: Option<String>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub created_at: String,
    pub updated_at: String,
    pub lease_until: Option<String>,
}

/// A job a runner claimed: what it needs to make the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claimed {
    pub id: String,
    pub kind: String,
    pub agent: String,
    pub model: String,
    pub settings: String,
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    pub json: bool,
    pub lease_until: String,
}

/// Why a runner's call was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    /// No such job for this member (or not in a state that takes this).
    NotFound,
    /// `MODEL_KEYS_SECRET` is not configured.
    NoSecret,
    /// The request was malformed: the message is a fixed sentence.
    Invalid(&'static str),
    /// The table could not be read or written.
    Store,
}

impl From<DbError> for JobError {
    fn from(_: DbError) -> Self {
        Self::Store
    }
}

// ---------------------------------------------------------------------------
// Runners

/// The member's runner for `role`: the role's own row, else their Main row.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn runner_for(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    role: &str,
) -> Result<Option<Runner>, DbError> {
    let rows = runners(db, workspace_id, user_id).await?;
    Ok(rows
        .iter()
        .find(|row| row.role == role)
        .or_else(|| rows.iter().find(|row| row.role == "main"))
        .cloned())
}

/// Every runner row of one member.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn runners(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Vec<Runner>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            "SELECT role, kind, agent, model, settings, updated_at, last_seen_at \
             FROM model_runners WHERE workspace_id = ? AND user_id = ? ORDER BY role",
            vec![text(workspace_id), text(user_id)],
        ))
        .await?;
    Ok(rows
        .rows
        .iter()
        .map(|row| Runner {
            role: string(row, "role"),
            kind: string(row, "kind"),
            agent: string(row, "agent"),
            model: string(row, "model"),
            settings: string(row, "settings"),
            updated_at: string(row, "updated_at"),
            last_seen_at: row.get::<Option<String>>("last_seen_at").flatten(),
        })
        .collect())
}

/// Sets (or replaces) a member's runner for one role.
///
/// # Errors
///
/// [`DbError`] from the database.
#[allow(clippy::too_many_arguments)]
pub async fn set_runner(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    user_id: &str,
    role: &str,
    agent: &str,
    model: &str,
    settings: &str,
) -> Result<(), DbError> {
    db.execute(&Statement::with_values(
        "INSERT INTO model_runners \
            (workspace_id, user_id, role, kind, agent, model, settings, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id, user_id, role) DO UPDATE SET \
            kind = excluded.kind, agent = excluded.agent, model = excluded.model, \
            settings = excluded.settings, updated_at = excluded.updated_at",
        vec![
            text(workspace_id),
            text(user_id),
            text(role),
            text(LOCAL_RUNNER),
            text(agent),
            text(model),
            text(settings),
            text(&stamp(now)),
        ],
    ))
    .await
    .map(|_| ())
}

/// Removes a member's runner for one role.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn delete_runner(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    role: &str,
) -> Result<(), DbError> {
    db.execute(&Statement::with_values(
        "DELETE FROM model_runners WHERE workspace_id = ? AND user_id = ? AND role = ?",
        vec![text(workspace_id), text(user_id), text(role)],
    ))
    .await
    .map(|_| ())
}

/// Whether a model would serve `owner`'s job for `role`: their own runner,
/// or the workspace's connection. Makes no call.
///
/// # Errors
///
/// [`InvokeError::Store`] when a table cannot be read.
pub async fn available(
    db: &dyn Database,
    workspace_id: &str,
    owner: &str,
    role: &str,
) -> Result<bool, InvokeError> {
    if runner_for(db, workspace_id, owner, role)
        .await
        .map_err(|_| InvokeError::Store)?
        .is_some()
    {
        return Ok(true);
    }
    Ok(invoke::serving(db, workspace_id, role).await?.is_some())
}

// ---------------------------------------------------------------------------
// The caller's side

/// The ports [`dispatch`] needs.
pub struct Ports<'a> {
    pub db: &'a dyn Database,
    pub http: &'a dyn HttpClient,
    pub config: &'a dyn Config,
    pub clock: &'a dyn Clock,
    pub id_gen: &'a dyn IdGen,
}

/// Runs one call for `call.owner`: on their runner when they have one for
/// `role`, otherwise on the workspace's connection ([`invoke::chat`]).
///
/// On the runner path the same call asked again finds its own job: an
/// answer that arrived is handed back (and the job consumed, its sealed
/// prompt and answer wiped); a job still waiting is [`Dispatch::Pending`];
/// a failed one is that failure, once, so a retry asks afresh. A call whose
/// prompt changed expires the older waiting job of the same `ref_key`.
///
/// # Errors
///
/// [`InvokeError`] — [`InvokeError::Runner`] carries the reason a runner
/// gave for a failed job.
pub async fn dispatch(
    ports: &Ports<'_>,
    workspace_id: &str,
    role: &str,
    call: &JobCall<'_>,
    request: &ChatRequest<'_>,
) -> Result<Dispatch, InvokeError> {
    let runner = runner_for(ports.db, workspace_id, call.owner, role)
        .await
        .map_err(|_| InvokeError::Store)?;
    let Some(runner) = runner else {
        return invoke::chat(
            ports.db,
            ports.http,
            ports.config,
            ports.clock,
            workspace_id,
            role,
            request,
        )
        .await
        .map(Dispatch::Reply);
    };
    let key = crypto::derive_key(ports.config).ok_or(InvokeError::NoSecret)?;
    let now = ports.clock.now();
    let sha = prompt_sha256(request, &runner);
    let found = ports
        .db
        .query(&Statement::with_values(
            format!(
                "SELECT {COLUMNS}, result FROM model_jobs \
                 WHERE workspace_id = ? AND owner = ? AND kind = ? AND ref_key = ? \
                   AND prompt_sha256 = ? AND status IN ('queued', 'claimed', 'done', 'failed') \
                 ORDER BY created_at DESC LIMIT 1"
            ),
            vec![
                text(workspace_id),
                text(call.owner),
                text(call.kind),
                text(call.ref_key),
                text(&sha),
            ],
        ))
        .await
        .map_err(|_| InvokeError::Store)?;
    if let Some(row) = found.first() {
        let info = info_from(row);
        let expired = string(row, "expires_at") <= stamp(now);
        match info.status.as_str() {
            "done" => {
                let blob = row.get::<Vec<u8>>("result").unwrap_or_default();
                let opened = crypto::decrypt(&key, &blob, workspace_id, &aad_role(&info.id))
                    .ok()
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                close(ports.db, now, &info.id, "consumed", None).await?;
                let text = opened.ok_or(InvokeError::Key)?;
                return Ok(Dispatch::Reply(ChatReply {
                    text,
                    prompt_tokens: info.prompt_tokens,
                    completion_tokens: info.completion_tokens,
                    usage_reported: info.prompt_tokens + info.completion_tokens > 0,
                    role: runner.role,
                    provider: format!("local-runner:{}", runner.agent),
                    model: runner.model,
                    runner: true,
                    truncated: false,
                }));
            }
            "failed" => {
                close(ports.db, now, &info.id, "consumed", info.error.as_deref()).await?;
                return Err(InvokeError::Runner(
                    info.error.unwrap_or_else(|| "cli-error".to_owned()),
                ));
            }
            _ if !expired => return Ok(Dispatch::Pending { job_id: info.id }),
            _ => {}
        }
    }
    // A new call: anything still waiting under the same handle was asked
    // with a prompt that is no longer current.
    ports
        .db
        .execute(&Statement::with_values(
            "UPDATE model_jobs SET status = 'expired', payload = NULL, result = NULL, \
                    lease_until = NULL, updated_at = ? \
             WHERE workspace_id = ? AND owner = ? AND kind = ? AND ref_key = ? \
               AND status IN ('queued', 'claimed', 'done')",
            vec![
                text(&stamp(now)),
                text(workspace_id),
                text(call.owner),
                text(call.kind),
                text(call.ref_key),
            ],
        ))
        .await
        .map_err(|_| InvokeError::Store)?;
    let id = format!("mj-{}", ports.id_gen.ulid().to_ascii_lowercase());
    let payload = serde_json::json!({"system": request.system, "user": request.user}).to_string();
    let sealed = crypto::encrypt(
        &key,
        payload.as_bytes(),
        workspace_id,
        &aad_role(&id),
        ports.id_gen,
    )
    .map_err(|_| InvokeError::Key)?;
    ports
        .db
        .execute(&Statement::with_values(
            "INSERT INTO model_jobs \
                (id, workspace_id, owner, role, kind, ref_key, prompt_sha256, agent, model, \
                 settings, max_tokens, json, payload, status, attempts, prompt_tokens, \
                 completion_tokens, created_at, updated_at, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'queued', 0, 0, 0, ?, ?, ?)",
            vec![
                text(&id),
                text(workspace_id),
                text(call.owner),
                text(&runner.role),
                text(call.kind),
                text(call.ref_key),
                text(&sha),
                text(&runner.agent),
                text(&runner.model),
                text(&runner.settings),
                Value::BigInt(Some(i64::from(request.max_tokens))),
                Value::BigInt(Some(i64::from(request.json))),
                Value::Bytes(Some(Box::new(sealed.blob))),
                text(&stamp(now)),
                text(&stamp(now)),
                text(&stamp(now + TTL)),
            ],
        ))
        .await
        .map_err(|_| InvokeError::Store)?;
    Ok(Dispatch::Pending { job_id: id })
}

// ---------------------------------------------------------------------------
// The runner's side

/// Claims the oldest of `owner`'s waiting jobs for their runner, and notes
/// that the runner was seen. `None` when nothing waits.
///
/// # Errors
///
/// [`JobError::NoSecret`] when the prompt cannot be opened, [`JobError::Store`].
pub async fn claim_next(
    db: &dyn Database,
    config: &dyn Config,
    now: OffsetDateTime,
    workspace_id: &str,
    owner: &str,
) -> Result<Option<Claimed>, JobError> {
    let key = crypto::derive_key(config).ok_or(JobError::NoSecret)?;
    let at = stamp(now);
    seen(db, &at, workspace_id, owner).await?;
    // Housekeeping first, on this owner's rows only: jobs past their TTL
    // expire, and a lapsed lease with no attempts left fails.
    db.execute(&Statement::with_values(
        "UPDATE model_jobs SET status = 'expired', payload = NULL, result = NULL, \
                lease_until = NULL, updated_at = ? \
         WHERE workspace_id = ? AND owner = ? AND status IN ('queued', 'claimed', 'done') \
           AND expires_at <= ?",
        vec![text(&at), text(workspace_id), text(owner), text(&at)],
    ))
    .await?;
    db.execute(&Statement::with_values(
        "UPDATE model_jobs SET status = 'failed', error = 'timeout', payload = NULL, \
                lease_until = NULL, updated_at = ? \
         WHERE workspace_id = ? AND owner = ? AND status = 'claimed' AND lease_until < ? \
           AND attempts >= ?",
        vec![
            text(&at),
            text(workspace_id),
            text(owner),
            text(&at),
            Value::BigInt(Some(MAX_ATTEMPTS)),
        ],
    ))
    .await?;
    let candidates = db
        .query(&Statement::with_values(
            format!(
                "SELECT {COLUMNS}, payload, settings, max_tokens, json FROM model_jobs \
                 WHERE workspace_id = ? AND owner = ? \
                   AND (status = 'queued' OR (status = 'claimed' AND lease_until < ?)) \
                 ORDER BY created_at ASC LIMIT 5"
            ),
            vec![text(workspace_id), text(owner), text(&at)],
        ))
        .await?;
    for row in &candidates.rows {
        let info = info_from(row);
        let lease_until = stamp(now + LEASE);
        let changed = db
            .execute(&Statement::with_values(
                "UPDATE model_jobs SET status = 'claimed', attempts = attempts + 1, \
                        lease_until = ?, updated_at = ? \
                 WHERE id = ? AND workspace_id = ? AND owner = ? AND status = ? \
                   AND updated_at = ?",
                vec![
                    text(&lease_until),
                    text(&at),
                    text(&info.id),
                    text(workspace_id),
                    text(owner),
                    text(&info.status),
                    text(&info.updated_at),
                ],
            ))
            .await?;
        if changed != 1 {
            continue;
        }
        let blob = row.get::<Vec<u8>>("payload").unwrap_or_default();
        let opened = crypto::decrypt(&key, &blob, workspace_id, &aad_role(&info.id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        let Some(payload) = opened else {
            close(db, now, &info.id, "failed", Some("unreadable"))
                .await
                .map_err(|_| JobError::Store)?;
            continue;
        };
        let field = |name: &str| {
            payload
                .get(name)
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        return Ok(Some(Claimed {
            id: info.id,
            kind: info.kind,
            agent: info.agent,
            model: info.model,
            settings: string(row, "settings"),
            system: field("system"),
            user: field("user"),
            max_tokens: row
                .get::<i64>("max_tokens")
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0),
            json: row.get::<i64>("json").unwrap_or_default() != 0,
            lease_until,
        }));
    }
    Ok(None)
}

/// Stores a runner's answer to one of `owner`'s claimed jobs. The text is
/// the caller's to have validated and redacted already; this seals it.
///
/// # Errors
///
/// [`JobError::NotFound`] for a job that is not `owner`'s or not claimed,
/// [`JobError::Invalid`] for an oversized answer, [`JobError::NoSecret`],
/// [`JobError::Store`].
#[allow(clippy::too_many_arguments)]
pub async fn complete(
    db: &dyn Database,
    config: &dyn Config,
    id_gen: &dyn IdGen,
    now: OffsetDateTime,
    workspace_id: &str,
    owner: &str,
    id: &str,
    text_out: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Result<JobInfo, JobError> {
    if text_out.len() > MAX_RESULT_BYTES {
        return Err(JobError::Invalid("the answer is too large"));
    }
    let key = crypto::derive_key(config).ok_or(JobError::NoSecret)?;
    let sealed = crypto::encrypt(
        &key,
        text_out.as_bytes(),
        workspace_id,
        &aad_role(id),
        id_gen,
    )
    .map_err(|_| JobError::Store)?;
    let changed = db
        .execute(&Statement::with_values(
            "UPDATE model_jobs SET status = 'done', result = ?, payload = NULL, \
                    lease_until = NULL, error = NULL, prompt_tokens = ?, \
                    completion_tokens = ?, updated_at = ? \
             WHERE id = ? AND workspace_id = ? AND owner = ? AND status = 'claimed'",
            vec![
                Value::Bytes(Some(Box::new(sealed.blob))),
                big(prompt_tokens),
                big(completion_tokens),
                text(&stamp(now)),
                text(id),
                text(workspace_id),
                text(owner),
            ],
        ))
        .await?;
    if changed != 1 {
        return Err(JobError::NotFound);
    }
    get(db, workspace_id, owner, id)
        .await?
        .ok_or(JobError::NotFound)
}

/// Records that one of `owner`'s claimed jobs failed, with a reason from
/// [`FAIL_REASONS`] (anything else is stored as `cli-error`).
///
/// # Errors
///
/// [`JobError::NotFound`], [`JobError::Store`].
pub async fn fail(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    owner: &str,
    id: &str,
    reason: &str,
) -> Result<JobInfo, JobError> {
    let reason = if FAIL_REASONS.contains(&reason) {
        reason
    } else {
        "cli-error"
    };
    let changed = db
        .execute(&Statement::with_values(
            "UPDATE model_jobs SET status = 'failed', error = ?, payload = NULL, result = NULL, \
                    lease_until = NULL, updated_at = ? \
             WHERE id = ? AND workspace_id = ? AND owner = ? AND status = 'claimed'",
            vec![
                text(reason),
                text(&stamp(now)),
                text(id),
                text(workspace_id),
                text(owner),
            ],
        ))
        .await?;
    if changed != 1 {
        return Err(JobError::NotFound);
    }
    get(db, workspace_id, owner, id)
        .await?
        .ok_or(JobError::NotFound)
}

/// One of `owner`'s jobs, or `None` — for anybody else's id as for an id
/// that does not exist.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn get(
    db: &dyn Database,
    workspace_id: &str,
    owner: &str,
    id: &str,
) -> Result<Option<JobInfo>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {COLUMNS} FROM model_jobs WHERE id = ? AND workspace_id = ? AND owner = ?"
            ),
            vec![text(id), text(workspace_id), text(owner)],
        ))
        .await?;
    Ok(rows.first().map(info_from))
}

/// `owner`'s most recent jobs, newest first.
///
/// # Errors
///
/// [`DbError`] from the database.
pub async fn list(
    db: &dyn Database,
    workspace_id: &str,
    owner: &str,
    limit: usize,
) -> Result<Vec<JobInfo>, DbError> {
    let rows = db
        .query(&Statement::with_values(
            format!(
                "SELECT {COLUMNS} FROM model_jobs WHERE workspace_id = ? AND owner = ? \
                 ORDER BY created_at DESC LIMIT ?"
            ),
            vec![
                text(workspace_id),
                text(owner),
                Value::BigInt(Some(i64::try_from(limit).unwrap_or(i64::MAX))),
            ],
        ))
        .await?;
    Ok(rows.rows.iter().map(info_from).collect())
}

// ---------------------------------------------------------------------------

/// The columns [`info_from`] reads.
const COLUMNS: &str = "id, owner, role, kind, ref_key, agent, model, status, attempts, error, \
     prompt_tokens, completion_tokens, created_at, updated_at, lease_until, expires_at";

fn info_from(row: &Row) -> JobInfo {
    let count = |name: &str| {
        row.get::<i64>(name)
            .and_then(|n| u64::try_from(n).ok())
            .unwrap_or(0)
    };
    JobInfo {
        id: string(row, "id"),
        owner: string(row, "owner"),
        role: string(row, "role"),
        kind: string(row, "kind"),
        ref_key: string(row, "ref_key"),
        agent: string(row, "agent"),
        model: string(row, "model"),
        status: string(row, "status"),
        attempts: row.get::<i64>("attempts").unwrap_or_default(),
        error: row.get::<Option<String>>("error").flatten(),
        prompt_tokens: count("prompt_tokens"),
        completion_tokens: count("completion_tokens"),
        created_at: string(row, "created_at"),
        updated_at: string(row, "updated_at"),
        lease_until: row.get::<Option<String>>("lease_until").flatten(),
    }
}

/// Ends a job: its sealed prompt and answer are wiped.
async fn close(
    db: &dyn Database,
    now: OffsetDateTime,
    id: &str,
    status: &str,
    error: Option<&str>,
) -> Result<(), InvokeError> {
    db.execute(&Statement::with_values(
        "UPDATE model_jobs SET status = ?, error = ?, payload = NULL, result = NULL, \
                lease_until = NULL, updated_at = ? WHERE id = ?",
        vec![
            text(status),
            Value::String(error.map(|e| Box::new(e.to_owned()))),
            text(&stamp(now)),
            text(id),
        ],
    ))
    .await
    .map(|_| ())
    .map_err(|_| InvokeError::Store)
}

/// Notes that a member's runner asked for work — at most once a minute, so a
/// runner polling every few seconds is not a write every few seconds.
async fn seen(db: &dyn Database, at: &str, workspace_id: &str, owner: &str) -> Result<(), DbError> {
    let minute_ago = OffsetDateTime::parse(at, &Rfc3339)
        .map(|t| stamp(t - Duration::minutes(1)))
        .unwrap_or_default();
    db.execute(&Statement::with_values(
        "UPDATE model_runners SET last_seen_at = ? \
         WHERE workspace_id = ? AND user_id = ? \
           AND (last_seen_at IS NULL OR last_seen_at < ?)",
        vec![text(at), text(workspace_id), text(owner), text(&minute_ago)],
    ))
    .await
    .map(|_| ())
}

/// The AAD "role" a job's ciphertexts are bound to.
fn aad_role(id: &str) -> String {
    format!("job:{id}")
}

/// What makes two calls the same call: the prompt, its limits, and the
/// runner it would go to.
fn prompt_sha256(request: &ChatRequest<'_>, runner: &Runner) -> String {
    let mut hasher = Sha256::new();
    for part in [
        request.system,
        request.user,
        &request.max_tokens.to_string(),
        if request.json { "json" } else { "text" },
        &runner.agent,
        &runner.model,
        &runner.settings,
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0u8]);
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// RFC 3339 in UTC at whole seconds, fixed width, so text order is time
/// order (the compiler's queue uses the same form).
fn stamp(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    let at = at.replace_nanosecond(0).unwrap_or(at);
    at.format(&Rfc3339).unwrap_or_default()
}

fn string(row: &Row, name: &str) -> String {
    row.get::<String>(name).unwrap_or_default()
}

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

fn big(value: u64) -> Value {
    Value::BigInt(Some(i64::try_from(value).unwrap_or(i64::MAX)))
}
