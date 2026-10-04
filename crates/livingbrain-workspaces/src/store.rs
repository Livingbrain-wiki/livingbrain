//! The two tables, behind the `Database` port.
//!
//! Every statement is parameterized — no value is ever interpolated into
//! SQL — and every one names its workspace, so a row can only be reached
//! through the workspace the caller's verified session carries.

use cratefield_core::{Clock, Database, DbError, Row, Statement};
use sea_query::Value;
use serde_json::Value as Json;

/// A workspace row: one Slack team, and the person who first signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub id: String,
    pub name: String,
    pub owner_id: String,
}

/// A member row: one Slack user within one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub user_id: String,
    pub name: String,
    pub timezone: Option<String>,
    pub is_admin: bool,
}

/// What [`apply_user_change`] did with an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserChange {
    /// The event named a known workspace, and its member row was upserted.
    Applied,
    /// The event was not a `user_change`, carried no usable user, or named
    /// a workspace this deployment has never seen. Nothing was written.
    Ignored,
}

/// Creates the workspace if this is the first sign-in, and does nothing at
/// all if it already exists.
///
/// This single statement is how the workspace gets exactly one owner: the
/// caller passes the signing-in user's id, the conflict clause discards
/// the insert when the row is already there, and no code path anywhere in
/// this module writes `owner_id` again.
pub(crate) async fn ensure_workspace(
    db: &dyn Database,
    id: &str,
    name: &str,
    owner_id: &str,
    now: &str,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO workspaces (id, name, owner_id, created_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT (id) DO NOTHING",
        vec![text(id), text(name), text(owner_id), text(now)],
    );
    db.execute(&statement).await.map(|_| ())
}

/// The workspace row, or `None` when there is no such workspace.
pub(crate) async fn workspace(db: &dyn Database, id: &str) -> Result<Option<Workspace>, DbError> {
    let statement = Statement::with_values(
        "SELECT id, name, owner_id FROM workspaces WHERE id = ?",
        vec![text(id)],
    );
    Ok(db.query(&statement).await?.first().map(workspace_from))
}

/// One member of one workspace, or `None`.
pub(crate) async fn member(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Option<Member>, DbError> {
    let statement = Statement::with_values(
        "SELECT user_id, name, timezone, is_admin FROM workspace_members \
         WHERE workspace_id = ? AND user_id = ?",
        vec![text(workspace_id), text(user_id)],
    );
    Ok(db.query(&statement).await?.first().map(member_from))
}

/// Every member of one workspace, ordered by user id so the answer is
/// stable across engines.
pub(crate) async fn members(db: &dyn Database, workspace_id: &str) -> Result<Vec<Member>, DbError> {
    let statement = Statement::with_values(
        "SELECT user_id, name, timezone, is_admin FROM workspace_members \
         WHERE workspace_id = ? ORDER BY user_id",
        vec![text(workspace_id)],
    );
    Ok(db
        .query(&statement)
        .await?
        .rows
        .iter()
        .map(member_from)
        .collect())
}

/// What one member row should say. Every field is optional and means "we
/// were not told" when absent — the row is created from what is here and an
/// existing row keeps what an absent field did not replace.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct MemberFields<'a> {
    pub name: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub is_admin: Option<bool>,
}

/// Creates the member row or refreshes the fields the caller named.
///
/// Both writers go through here. Sign-in passes a name and nothing else,
/// because the `id_token` carries no timezone or admin flag; a `user_change`
/// passes whatever Slack sent. Absent fields are left alone on the update —
/// so signing in again cannot undo what a `user_change` has since told us,
/// and a partial event cannot blank a name — and a new row takes `NULL`/
/// `0` for the two Slack did not supply.
pub(crate) async fn upsert_member(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    fields: MemberFields<'_>,
    now: &str,
) -> Result<(), DbError> {
    let MemberFields {
        name,
        timezone,
        is_admin,
    } = fields;
    let insert = Statement::with_values(
        "INSERT INTO workspace_members (workspace_id, user_id, name, timezone, is_admin, updated_at) \
         VALUES (?, ?, COALESCE(?, ''), ?, COALESCE(?, 0), ?) \
         ON CONFLICT (workspace_id, user_id) DO NOTHING",
        vec![
            text(workspace_id),
            text(user_id),
            nullable_text(name.filter(|name| !name.is_empty())),
            nullable_text(timezone.filter(|timezone| !timezone.is_empty())),
            nullable_flag(is_admin),
            text(now),
        ],
    );
    db.execute(&insert).await?;

    let update = Statement::with_values(
        "UPDATE workspace_members \
            SET name = COALESCE(NULLIF(?, ''), name), \
                timezone = COALESCE(?, timezone), \
                is_admin = COALESCE(?, is_admin), \
                updated_at = ? \
          WHERE workspace_id = ? AND user_id = ?",
        vec![
            text(name.unwrap_or_default()),
            nullable_text(timezone.filter(|timezone| !timezone.is_empty())),
            nullable_flag(is_admin),
            text(now),
            text(workspace_id),
            text(user_id),
        ],
    );
    db.execute(&update).await.map(|_| ())
}

/// Applies a Slack `user_change` event to the member mirror.
///
/// This is the seam issue #6's Events webhook calls; the webhook itself —
/// `POST /slack/events`, its signing secret, the url_verification
/// handshake — is that issue's, and is deliberately not built here.
///
/// **The caller must have verified the event first, and must pass the
/// `team_id` from the signed envelope** (the payload's `team_id`, or the
/// `authorizations[].team_id` Slack sends for an org-wide app) — never
/// `event["user"]["team_id"]`, which is attacker-controlled text until the
/// signature check says otherwise. That is why the workspace is a parameter
/// and not read out of the event: an event whose `user.team_id` is present
/// and disagrees is ignored here, but the parameter is what the row is
/// keyed on either way. An event for a workspace this deployment has never
/// seen is ignored too: a `user_change` must not be able to conjure a
/// workspace, because only a sign-in decides who owns one.
///
/// The event shape it reads is `{"type":"user_change","user":{"id","name",
/// "real_name","tz","is_admin","profile":{"real_name","display_name"}}}`.
/// A field the event omits is left alone, so a partial event can neither
/// demote an admin nor move a timezone nor blank a name. `deleted` users
/// are not removed; nothing here deletes rows.
///
/// # Errors
///
/// A [`DbError`] when the read or the write fails.
pub async fn apply_user_change(
    db: &dyn Database,
    clock: &dyn Clock,
    team_id: &str,
    event: &Json,
) -> Result<UserChange, DbError> {
    if event.get("type").and_then(Json::as_str) != Some("user_change") {
        return Ok(UserChange::Ignored);
    }
    let Some(user) = event.get("user") else {
        return Ok(UserChange::Ignored);
    };
    // The envelope's team id is authoritative. A `user` block naming a
    // different workspace is not ours to act on.
    if matches!(user.get("team_id").and_then(Json::as_str), Some(other) if other != team_id) {
        return Ok(UserChange::Ignored);
    }
    let Some(user_id) = user
        .get("id")
        .and_then(Json::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(UserChange::Ignored);
    };
    if team_id.is_empty() || workspace(db, team_id).await?.is_none() {
        return Ok(UserChange::Ignored);
    }

    upsert_member(
        db,
        team_id,
        user_id,
        MemberFields {
            name: Some(display_name(user).as_str()),
            timezone: user.get("tz").and_then(Json::as_str),
            is_admin: user.get("is_admin").and_then(Json::as_bool),
        },
        &now(clock),
    )
    .await?;
    Ok(UserChange::Applied)
}

/// The timestamp written to `updated_at`, from the [`Clock`] port so a test
/// can pin it.
fn now(clock: &dyn Clock) -> String {
    clock
        .now()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The most human name Slack sent, preferring the profile's real name.
fn display_name(user: &Json) -> String {
    let profile = user.get("profile");
    let from_profile = |key: &str| profile.and_then(|p| p.get(key)).and_then(Json::as_str);
    [
        from_profile("real_name"),
        user.get("real_name").and_then(Json::as_str),
        from_profile("display_name"),
        user.get("name").and_then(Json::as_str),
    ]
    .into_iter()
    .flatten()
    .find(|name| !name.is_empty())
    .unwrap_or_default()
    .to_owned()
}

/// A non-null text bind.
fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

/// A text bind that is SQL NULL when there is no value.
fn nullable_text(value: Option<&str>) -> Value {
    value.map_or(Value::String(None), text)
}

/// A boolean stored as the portable 0/1 INTEGER flag (ADR 0004), or SQL
/// NULL when the event did not say.
fn nullable_flag(value: Option<bool>) -> Value {
    value.map_or(Value::BigInt(None), |flag| {
        Value::BigInt(Some(i64::from(flag)))
    })
}

fn workspace_from(row: &Row) -> Workspace {
    Workspace {
        id: row.get::<String>("id").unwrap_or_default(),
        name: row.get::<String>("name").unwrap_or_default(),
        owner_id: row.get::<String>("owner_id").unwrap_or_default(),
    }
}

fn member_from(row: &Row) -> Member {
    Member {
        user_id: row.get::<String>("user_id").unwrap_or_default(),
        name: row.get::<String>("name").unwrap_or_default(),
        timezone: row.get::<Option<String>>("timezone").flatten(),
        is_admin: row.get::<i64>("is_admin").unwrap_or_default() != 0,
    }
}
