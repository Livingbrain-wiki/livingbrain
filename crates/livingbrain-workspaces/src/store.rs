//! The tables, behind the `Database` port.
//!
//! Every statement is parameterized — no value is ever interpolated into
//! SQL — and every one names its workspace, so a row can only be reached
//! through the workspace the caller's verified session carries.
//!
//! Since ADR 0002 the workspace id is ours and opaque, and the two tables
//! that bind a chat platform to one are `workspace_connections` and
//! `member_identities`. [`link_connection`] and [`link_identity`] are the
//! public seams over them: a caller that has *verified* a platform id binds
//! it here. No route in this module accepts a platform id it has not
//! verified first, and neither seam ever rewrites a row that exists.

use cratefield_core::{Clock, Database, DbError, Row, Statement};
use sea_query::Value;
use serde_json::Value as Json;

/// A workspace row: one tenant, and the person who first signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Workspace {
    pub id: String,
    pub name: String,
    pub owner_id: String,
}

/// A member row: one person within one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Member {
    pub user_id: String,
    pub name: String,
    pub timezone: Option<String>,
    /// Slack's admin flag, mirrored. Not the member's role: see [`Role`].
    pub is_admin: bool,
    /// What the member may do here (ADR 0004).
    pub role: Role,
}

impl Member {
    /// Whether the member may administer the workspace: an owner or admin
    /// role, or Slack's admin flag (kept so nobody who was an admin before
    /// roles existed stops being one).
    pub(crate) fn may_administer(&self) -> bool {
        matches!(self.role, Role::Owner | Role::Admin) || self.is_admin
    }
}

/// A member's role (ADR 0004). Ordered by reach: an owner may do everything
/// an admin may, and so on down to a viewer, who reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// Reads the shared space; writes only their own.
    Viewer,
    /// Reads and writes the shared space.
    Member,
    /// Manages members (not owners), invitations and agent accounts.
    Admin,
    /// Everything, including other owners. A workspace keeps at least one.
    Owner,
}

impl Role {
    /// The stored spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Member => "member",
            Self::Admin => "admin",
            Self::Owner => "owner",
        }
    }

    /// The role a stored or requested spelling names, or `None`.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "viewer" => Some(Self::Viewer),
            "member" => Some(Self::Member),
            "admin" => Some(Self::Admin),
            "owner" => Some(Self::Owner),
            _ => None,
        }
    }
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

/// What [`link_connection`] did with a platform's id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkOutcome {
    /// The connection was created.
    Linked,
    /// This workspace already had this exact platform id linked. Signing in
    /// again, or re-linking what is already linked, lands here — and is a
    /// success, not an error: the caller asked for a state they are in.
    AlreadyThisWorkspace,
    /// The platform id is already linked to *another* workspace. The row is
    /// left exactly as it was; a clash is the caller's to resolve (ADR
    /// 0002: a clash answers 409).
    Conflict,
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
        "SELECT user_id, name, timezone, is_admin, role FROM workspace_members \
         WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL",
        vec![text(workspace_id), text(user_id)],
    );
    Ok(db.query(&statement).await?.first().map(member_from))
}

/// Every member of one workspace, ordered by user id so the answer is
/// stable across engines.
pub(crate) async fn members(db: &dyn Database, workspace_id: &str) -> Result<Vec<Member>, DbError> {
    let statement = Statement::with_values(
        "SELECT user_id, name, timezone, is_admin, role FROM workspace_members \
         WHERE workspace_id = ? AND removed_at IS NULL ORDER BY user_id",
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
        "INSERT INTO workspace_members \
           (workspace_id, user_id, name, timezone, is_admin, updated_at, role) \
         VALUES (?, ?, COALESCE(?, ''), ?, COALESCE(?, 0), ?, \
           CASE WHEN EXISTS (SELECT 1 FROM workspaces w WHERE w.id = ? AND w.owner_id = ?) \
                THEN 'owner' ELSE 'member' END) \
         ON CONFLICT (workspace_id, user_id) DO NOTHING",
        vec![
            text(workspace_id),
            text(user_id),
            nullable_text(name.filter(|name| !name.is_empty())),
            nullable_text(timezone.filter(|timezone| !timezone.is_empty())),
            nullable_flag(is_admin),
            text(now),
            text(workspace_id),
            text(user_id),
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

/// Binds a chat platform's workspace id to a workspace, if it is free.
///
/// **The caller must have verified the platform id first** and must never
/// pass a client-supplied one. That is the whole contract of the seam
/// (ADR 0002): `external_id` is the team id out of a verified Slack
/// `id_token`, the guild id a future Discord OAuth flow verified, or the
/// address the holder of a sign-in token proved by clicking it.
///
/// The insert is the authority and its `UNIQUE (platform, external_id)`
/// violation is not swallowed: on any error the row that holds that
/// `(platform, external_id)` is read back, and only then is the outcome
/// named. Nothing here rewrites an existing row — a workspace that already
/// has a Slack connection keeps the team it had, even when a *different*
/// team is offered for it.
///
/// # Errors
///
/// A [`DbError`] when the insert or the read-back fails, and when the
/// insert failed for a reason that is not a link conflict (a workspace
/// already holding a connection for this platform, say), which propagates
/// rather than being reported as a `LinkOutcome` it is not.
pub async fn link_connection(
    db: &dyn Database,
    workspace_id: &str,
    platform: &str,
    external_id: &str,
    now: &str,
) -> Result<LinkOutcome, DbError> {
    let insert = Statement::with_values(
        "INSERT INTO workspace_connections (workspace_id, platform, external_id, created_at) \
         VALUES (?, ?, ?, ?)",
        vec![
            text(workspace_id),
            text(platform),
            text(external_id),
            text(now),
        ],
    );
    match db.execute(&insert).await {
        Ok(_) => Ok(LinkOutcome::Linked),
        Err(error) => match workspace_for_platform(db, platform, external_id).await? {
            Some(owner) if owner == workspace_id => Ok(LinkOutcome::AlreadyThisWorkspace),
            Some(_) => Ok(LinkOutcome::Conflict),
            None => Err(error),
        },
    }
}

/// The workspace a chat platform's workspace id is linked to, or `None`
/// when nothing is linked to it.
///
/// Every Slack lookup goes through here rather than parsing a workspace id
/// as a team id, which is what ADR 0001 did and ADR 0002 supersedes.
pub(crate) async fn workspace_for_platform(
    db: &dyn Database,
    platform: &str,
    external_id: &str,
) -> Result<Option<String>, DbError> {
    let statement = Statement::with_values(
        "SELECT workspace_id FROM workspace_connections \
         WHERE platform = ? AND external_id = ?",
        vec![text(platform), text(external_id)],
    );
    Ok(db
        .query(&statement)
        .await?
        .first()
        .and_then(|row| row.get::<String>("workspace_id"))
        .filter(|id| !id.is_empty()))
}

/// Binds a chat platform's *user* id to a member of a workspace, if it is
/// free.
///
/// **The caller must have verified the platform's user id first**, exactly
/// as for [`link_connection`]. Re-linking an id this workspace already has
/// is a no-op rather than an error, so signing in repeatedly is safe; a
/// `user_id` that another member of this workspace already answers to is
/// left alone too — the row says the external id is spoken for, and which
/// way that is resolved is a question about the member mirror, not about
/// this seam.
pub async fn link_identity(
    db: &dyn Database,
    workspace_id: &str,
    platform: &str,
    external_id: &str,
    user_id: &str,
    now: &str,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO member_identities (workspace_id, platform, external_id, user_id, created_at) \
         VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id, platform, external_id) DO NOTHING",
        vec![
            text(workspace_id),
            text(platform),
            text(external_id),
            text(user_id),
            text(now),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// The member a chat platform's user id resolves to inside one workspace,
/// or `None` when the workspace does not know that person.
///
/// The join is what makes a sign-in link work: an email address resolves
/// through `member_identities` to a member, and only a member the
/// workspace already has may be signed in this way.
pub(crate) async fn member_for_identity(
    db: &dyn Database,
    workspace_id: &str,
    platform: &str,
    external_id: &str,
) -> Result<Option<Member>, DbError> {
    let statement = Statement::with_values(
        "SELECT m.user_id, m.name, m.timezone, m.is_admin, m.role FROM member_identities i \
         JOIN workspace_members m \
           ON m.workspace_id = i.workspace_id AND m.user_id = i.user_id \
         WHERE i.workspace_id = ? AND i.platform = ? AND i.external_id = ? \
           AND m.removed_at IS NULL",
        vec![text(workspace_id), text(platform), text(external_id)],
    );
    Ok(db.query(&statement).await?.first().map(member_from))
}

/// The bot user id Slack made for this team's installation, or `None` when
/// the team never installed the app.
///
/// Read beside [`crate::install::bot_token`] rather than with it: the token
/// has to be opened, which needs a key custodian, and refusing to recognise
/// one's own messages needs nothing but the id. A deployment with no key
/// ring can still drop the app's own echoes this way.
pub(crate) async fn bot_user_id(
    db: &dyn Database,
    team_id: &str,
) -> Result<Option<String>, DbError> {
    Ok(db
        .query(&Statement::with_values(
            "SELECT bot_user_id FROM slack_installs WHERE team_id = ?",
            vec![text(team_id)],
        ))
        .await?
        .first()
        .and_then(|row| row.get::<String>("bot_user_id"))
        .filter(|id| !id.is_empty()))
}

/// One unspent sign-in link, read by [`take_sign_in_link`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignInLinkRow {
    /// The address the link was minted for, normalized.
    pub email: String,
    /// The workspace to sign in to, or [`None`] to create one.
    pub workspace_id: Option<String>,
    /// The name for a workspace this link will create.
    pub workspace_name: String,
    /// ISO-8601, and compared as text: every writer formats it the same
    /// way, so an expired row is one that sorts before "now".
    pub expires_at: String,
}

/// Mints a sign-in link: the SHA-256 of a token, the address it signs in,
/// and when it stops working. The token itself is never stored, so a read
/// of this table cannot produce a link anybody can click.
///
/// `workspace_id` is [`None`] for a link that creates a workspace and the
/// workspace to sign into otherwise. `now` is part of the signature for
/// symmetry with the writers above; `sign_in_links` has no `created_at`
/// column, because a spent row is worth nothing and an unspent one is
/// bounded by `expires_at`.
pub(crate) async fn put_sign_in_link(
    db: &dyn Database,
    token_hash: &str,
    email: &str,
    workspace_id: Option<&str>,
    workspace_name: &str,
    expires_at: &str,
    _now: &str,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "INSERT INTO sign_in_links \
           (token_hash, email, workspace_id, workspace_name, expires_at) \
         VALUES (?, ?, ?, ?, ?)",
        vec![
            text(token_hash),
            text(email),
            nullable_text(workspace_id),
            text(workspace_name),
            text(expires_at),
        ],
    );
    db.execute(&statement).await.map(|_| ())
}

/// Spends a sign-in link, once, and returns what it was for.
///
/// The `UPDATE ... WHERE spent_at IS NULL` is the whole of it: the row is
/// marked spent by the first caller and every later caller updates nothing
/// and is told [`None`]. So a link cannot be spent twice even when two
/// requests race, and the read that follows is of a row this call is
/// already the one that consumed. The expiry is *not* applied here — an
/// expired link is spent too, so forwarding it cannot keep it alive — and
/// the caller compares [`SignInLinkRow::expires_at`] against the clock.
///
/// # Errors
///
/// A [`DbError`] when the update or the read fails.
pub(crate) async fn take_sign_in_link(
    db: &dyn Database,
    token_hash: &str,
    now: &str,
) -> Result<Option<SignInLinkRow>, DbError> {
    let spend = Statement::with_values(
        "UPDATE sign_in_links SET spent_at = ? \
         WHERE token_hash = ? AND spent_at IS NULL",
        vec![text(now), text(token_hash)],
    );
    if db.execute(&spend).await? == 0 {
        return Ok(None);
    }
    // The `spent_at = ?` on the read is a belt to the update's braces: a
    // caller that is not the one that spent the row cannot read it back.
    let read = Statement::with_values(
        "SELECT email, workspace_id, workspace_name, expires_at FROM sign_in_links \
         WHERE token_hash = ? AND spent_at = ?",
        vec![text(token_hash), text(now)],
    );
    Ok(db.query(&read).await?.first().map(sign_in_link_from))
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
/// signature check says otherwise. That is why the team is a parameter
/// and not read out of the event: an event whose `user.team_id` is present
/// and disagrees is ignored here, but the parameter is what the row is
/// keyed on either way. The parameter is a *Slack* team id and is resolved
/// to a workspace through `workspace_connections` (ADR 0002), never used as
/// a workspace id directly. An event for a team this deployment has never
/// linked is ignored too: a `user_change` must not be able to conjure a
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
    if team_id.is_empty() {
        return Ok(UserChange::Ignored);
    }
    // The team id is not the workspace id any more (ADR 0002), so it is
    // resolved through the connection table rather than used as a key. A
    // team this deployment has never linked resolves to nothing, and the
    // event is ignored: a `user_change` must not be able to conjure a
    // workspace, because only a sign-in decides who owns one.
    let Some(workspace_id) = workspace_for_platform(db, "slack", team_id).await? else {
        return Ok(UserChange::Ignored);
    };
    if workspace(db, &workspace_id).await?.is_none() {
        return Ok(UserChange::Ignored);
    }

    upsert_member(
        db,
        &workspace_id,
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
        // An unreadable role reads as the least one, so a corrupt row fails
        // closed rather than open.
        role: row
            .get::<String>("role")
            .and_then(|role| Role::parse(&role))
            .unwrap_or(Role::Viewer),
    }
}

fn sign_in_link_from(row: &Row) -> SignInLinkRow {
    SignInLinkRow {
        email: row.get::<String>("email").unwrap_or_default(),
        workspace_id: row
            .get::<Option<String>>("workspace_id")
            .flatten()
            .filter(|id| !id.is_empty()),
        workspace_name: row.get::<String>("workspace_name").unwrap_or_default(),
        expires_at: row.get::<String>("expires_at").unwrap_or_default(),
    }
}

// ---------------------------------------------------------------------------
// Roles, removal and invitations (ADR 0004, phase a)
//
// Every write that could leave a workspace without an owner is one guarded
// statement inside a `batch_atomic` whose first statement takes the
// workspace's row lock, so two owners demoting or removing each other at
// once cannot both succeed under Postgres `READ COMMITTED` (on D1 writes are
// serialized anyway). `batch_atomic` reports no row counts, so the caller
// reads the row back to learn which way it went.

/// The lock the guarded writes below take first: a no-op write on the
/// workspace row.
fn lock_workspace(workspace_id: &str) -> Statement {
    Statement::with_values(
        "UPDATE workspaces SET id = id WHERE id = ?",
        vec![text(workspace_id)],
    )
}

/// The `WHERE` fragment that holds an owner row in place while it is the
/// last live owner. Non-owner rows are never held by it.
const ANOTHER_OWNER_REMAINS: &str = "(role <> 'owner' OR \
     (SELECT COUNT(*) FROM workspace_members o \
       WHERE o.workspace_id = workspace_members.workspace_id \
         AND o.role = 'owner' AND o.removed_at IS NULL) > 1)";

/// What a role change or a removal did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RosterOutcome {
    /// Applied.
    Done,
    /// No live member with that id in this workspace.
    NotFound,
    /// Refused: it would leave the workspace with no owner.
    LastOwner,
}

/// Sets a live member's role, unless they are the last owner and the new
/// role is not `owner`.
pub(crate) async fn set_role(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    role: Role,
    now: &str,
) -> Result<RosterOutcome, DbError> {
    // Making somebody an owner can never leave the workspace without one, so
    // the guard is only on the way down. Decided here rather than as a bound
    // `? = 'owner'`, which Postgres cannot type.
    let guard = if role == Role::Owner {
        String::new()
    } else {
        format!(" AND {ANOTHER_OWNER_REMAINS}")
    };
    let update = Statement::with_values(
        format!(
            "UPDATE workspace_members SET role = ?, updated_at = ? \
              WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL{guard}"
        ),
        vec![
            text(role.as_str()),
            text(now),
            text(workspace_id),
            text(user_id),
        ],
    );
    db.batch_atomic(&[lock_workspace(workspace_id), update])
        .await?;
    Ok(match member(db, workspace_id, user_id).await? {
        None => RosterOutcome::NotFound,
        Some(row) if row.role == role => RosterOutcome::Done,
        Some(_) => RosterOutcome::LastOwner,
    })
}

/// Removes a live member: tombstones the row and deletes the identities that
/// sign them in, unless they are the last owner. Their sessions and tokens
/// stop working at once, because both re-read the member row on every
/// request and a removed row is no row.
pub(crate) async fn remove_member(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    now: &str,
) -> Result<RosterOutcome, DbError> {
    if member(db, workspace_id, user_id).await?.is_none() {
        return Ok(RosterOutcome::NotFound);
    }
    let tombstone = Statement::with_values(
        format!(
            "UPDATE workspace_members SET removed_at = ?, updated_at = ? \
              WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL \
                AND {ANOTHER_OWNER_REMAINS}"
        ),
        vec![text(now), text(now), text(workspace_id), text(user_id)],
    );
    // Only once the tombstone above landed: an identity row of somebody the
    // guard held back is left exactly where it was.
    let identities = Statement::with_values(
        "DELETE FROM member_identities WHERE workspace_id = ? AND user_id = ? \
           AND EXISTS (SELECT 1 FROM workspace_members m \
                        WHERE m.workspace_id = ? AND m.user_id = ? \
                          AND m.removed_at IS NOT NULL)",
        vec![
            text(workspace_id),
            text(user_id),
            text(workspace_id),
            text(user_id),
        ],
    );
    db.batch_atomic(&[lock_workspace(workspace_id), tombstone, identities])
        .await?;
    Ok(if member(db, workspace_id, user_id).await?.is_none() {
        RosterOutcome::Done
    } else {
        RosterOutcome::LastOwner
    })
}

/// The email addresses a member signs in with, in this workspace.
pub(crate) async fn email_identities(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
) -> Result<Vec<String>, DbError> {
    let statement = Statement::with_values(
        "SELECT external_id FROM member_identities \
          WHERE workspace_id = ? AND user_id = ? AND platform = 'email' \
          ORDER BY external_id",
        vec![text(workspace_id), text(user_id)],
    );
    Ok(db
        .query(&statement)
        .await?
        .rows
        .iter()
        .filter_map(|row| row.get::<String>("external_id"))
        .collect())
}

/// One workspace a person's address signs them in to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Membership {
    pub workspace_id: String,
    pub workspace_name: String,
    pub user_id: String,
    pub role: Role,
}

/// Every workspace where one of `addresses` is a live member's email
/// identity, ordered by name — the workspace switcher's list.
pub(crate) async fn memberships_for(
    db: &dyn Database,
    addresses: &[String],
) -> Result<Vec<Membership>, DbError> {
    if addresses.is_empty() {
        return Ok(Vec::new());
    }
    let marks = vec!["?"; addresses.len()].join(", ");
    let statement = Statement::with_values(
        format!(
            "SELECT DISTINCT w.id AS workspace_id, w.name AS workspace_name, \
                    m.user_id AS user_id, m.role AS role \
               FROM member_identities i \
               JOIN workspace_members m \
                 ON m.workspace_id = i.workspace_id AND m.user_id = i.user_id \
               JOIN workspaces w ON w.id = i.workspace_id \
              WHERE i.platform = 'email' AND i.external_id IN ({marks}) \
                AND m.removed_at IS NULL \
              ORDER BY workspace_name, workspace_id"
        ),
        addresses.iter().map(|address| text(address)).collect(),
    );
    Ok(db
        .query(&statement)
        .await?
        .rows
        .iter()
        .map(|row| Membership {
            workspace_id: row.get::<String>("workspace_id").unwrap_or_default(),
            workspace_name: row.get::<String>("workspace_name").unwrap_or_default(),
            user_id: row.get::<String>("user_id").unwrap_or_default(),
            role: row
                .get::<String>("role")
                .and_then(|role| Role::parse(&role))
                .unwrap_or(Role::Viewer),
        })
        .collect())
}

/// One invitation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Invite {
    pub id: String,
    pub email: String,
    pub role: Role,
    pub invited_by: String,
    pub created_at: String,
    pub expires_at: String,
    pub accepted_at: Option<String>,
    pub accepted_user_id: Option<String>,
    pub revoked_at: Option<String>,
}

const INVITE_COLUMNS: &str = "id, email, role, invited_by, created_at, expires_at, \
                              accepted_at, accepted_user_id, revoked_at";

fn invite_from(row: &Row) -> Invite {
    let optional = |name: &str| {
        row.get::<Option<String>>(name)
            .flatten()
            .filter(|value| !value.is_empty())
    };
    Invite {
        id: row.get::<String>("id").unwrap_or_default(),
        email: row.get::<String>("email").unwrap_or_default(),
        role: row
            .get::<String>("role")
            .and_then(|role| Role::parse(&role))
            .unwrap_or(Role::Viewer),
        invited_by: row.get::<String>("invited_by").unwrap_or_default(),
        created_at: row.get::<String>("created_at").unwrap_or_default(),
        expires_at: row.get::<String>("expires_at").unwrap_or_default(),
        accepted_at: optional("accepted_at"),
        accepted_user_id: optional("accepted_user_id"),
        revoked_at: optional("revoked_at"),
    }
}

/// What a new invitation records.
pub(crate) struct NewInvite<'a> {
    pub id: &'a str,
    pub workspace_id: &'a str,
    pub email: &'a str,
    pub role: Role,
    pub invited_by: &'a str,
    pub link_hash: &'a str,
    pub expires_at: &'a str,
}

/// Records an invitation. Any earlier invitation still open for the same
/// address in the same workspace is revoked in the same transaction (and
/// its link spent), so one address holds at most one live invitation.
pub(crate) async fn put_invite(
    db: &dyn Database,
    invite: &NewInvite<'_>,
    now: &str,
) -> Result<(), DbError> {
    let spend_old_links = Statement::with_values(
        "UPDATE sign_in_links SET spent_at = ? WHERE spent_at IS NULL AND token_hash IN \
           (SELECT link_hash FROM workspace_invites \
             WHERE workspace_id = ? AND email = ? \
               AND accepted_at IS NULL AND revoked_at IS NULL)",
        vec![text(now), text(invite.workspace_id), text(invite.email)],
    );
    let revoke_old = Statement::with_values(
        "UPDATE workspace_invites SET revoked_at = ? \
          WHERE workspace_id = ? AND email = ? AND accepted_at IS NULL AND revoked_at IS NULL",
        vec![text(now), text(invite.workspace_id), text(invite.email)],
    );
    let insert = Statement::with_values(
        "INSERT INTO workspace_invites \
           (id, workspace_id, email, role, invited_by, link_hash, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            text(invite.id),
            text(invite.workspace_id),
            text(invite.email),
            text(invite.role.as_str()),
            text(invite.invited_by),
            text(invite.link_hash),
            text(now),
            text(invite.expires_at),
        ],
    );
    db.batch_atomic(&[spend_old_links, revoke_old, insert])
        .await
}

/// The invitations of one workspace that are still open (not accepted, not
/// revoked, not expired), newest first.
pub(crate) async fn open_invites(
    db: &dyn Database,
    workspace_id: &str,
    now: &str,
) -> Result<Vec<Invite>, DbError> {
    let statement = Statement::with_values(
        format!(
            "SELECT {INVITE_COLUMNS} FROM workspace_invites \
              WHERE workspace_id = ? AND accepted_at IS NULL AND revoked_at IS NULL \
                AND expires_at > ? \
              ORDER BY created_at DESC, id"
        ),
        vec![text(workspace_id), text(now)],
    );
    Ok(db
        .query(&statement)
        .await?
        .rows
        .iter()
        .map(invite_from)
        .collect())
}

/// One invitation of this workspace, in any state.
pub(crate) async fn invite(
    db: &dyn Database,
    workspace_id: &str,
    id: &str,
) -> Result<Option<Invite>, DbError> {
    let statement = Statement::with_values(
        format!("SELECT {INVITE_COLUMNS} FROM workspace_invites WHERE workspace_id = ? AND id = ?"),
        vec![text(workspace_id), text(id)],
    );
    Ok(db.query(&statement).await?.first().map(invite_from))
}

/// Revokes an open invitation and spends its link. Answers whether one was
/// open.
pub(crate) async fn revoke_invite(
    db: &dyn Database,
    workspace_id: &str,
    id: &str,
    now: &str,
) -> Result<bool, DbError> {
    let Some(found) = invite(db, workspace_id, id).await? else {
        return Ok(false);
    };
    if found.accepted_at.is_some() || found.revoked_at.is_some() {
        return Ok(false);
    }
    db.batch_atomic(&[
        Statement::with_values(
            "UPDATE sign_in_links SET spent_at = ? WHERE spent_at IS NULL AND token_hash IN \
               (SELECT link_hash FROM workspace_invites WHERE workspace_id = ? AND id = ?)",
            vec![text(now), text(workspace_id), text(id)],
        ),
        Statement::with_values(
            "UPDATE workspace_invites SET revoked_at = ? \
              WHERE workspace_id = ? AND id = ? AND accepted_at IS NULL AND revoked_at IS NULL",
            vec![text(now), text(workspace_id), text(id)],
        ),
    ])
    .await?;
    Ok(true)
}

/// Accepts the open invitation for `email` in `workspace_id`, if there is
/// one: one guarded `UPDATE` claims it for `user_id`, so two accepts cannot
/// both win. Answers the role it carried.
pub(crate) async fn accept_invite(
    db: &dyn Database,
    workspace_id: &str,
    email: &str,
    user_id: &str,
    now: &str,
) -> Result<Option<Role>, DbError> {
    let claim = Statement::with_values(
        "UPDATE workspace_invites SET accepted_at = ?, accepted_user_id = ? \
          WHERE id = (SELECT id FROM workspace_invites \
                       WHERE workspace_id = ? AND email = ? AND accepted_at IS NULL \
                         AND revoked_at IS NULL AND expires_at > ? \
                       ORDER BY created_at DESC, id LIMIT 1) \
            AND accepted_at IS NULL AND revoked_at IS NULL",
        vec![
            text(now),
            text(user_id),
            text(workspace_id),
            text(email),
            text(now),
        ],
    );
    if db.execute(&claim).await? == 0 {
        return Ok(None);
    }
    let read = Statement::with_values(
        "SELECT role FROM workspace_invites \
          WHERE workspace_id = ? AND email = ? AND accepted_user_id = ? AND accepted_at = ?",
        vec![text(workspace_id), text(email), text(user_id), text(now)],
    );
    Ok(db
        .query(&read)
        .await?
        .first()
        .and_then(|row| row.get::<String>("role"))
        .and_then(|role| Role::parse(&role)))
}

/// Gives a member just created by an accepted invitation the role it
/// carried. Only ever called on a row this request created.
pub(crate) async fn set_initial_role(
    db: &dyn Database,
    workspace_id: &str,
    user_id: &str,
    role: Role,
) -> Result<(), DbError> {
    let statement = Statement::with_values(
        "UPDATE workspace_members SET role = ? \
          WHERE workspace_id = ? AND user_id = ? AND removed_at IS NULL",
        vec![text(role.as_str()), text(workspace_id), text(user_id)],
    );
    db.execute(&statement).await.map(|_| ())
}
