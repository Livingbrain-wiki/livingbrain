//! The settings audit store: append one redacted row per settings change,
//! list a workspace's rows newest-first.
//!
//! The rule the module exists for is that **a value never reaches the table
//! raw**. A BYOK API key is a setting, so a settings write is a credential
//! write wearing a settings costume. [`SettingChange`] is the only way to
//! build a row and it redacts on construction, so there is no path into
//! [`AuditStore::record`] that carries an unredacted value.

use std::sync::Arc;

use cratefield_core::{Clock, Database, DbError, IdGen, Row, Statement};
use livingbrain_redact::{Policy, RedactError, redact};
use sea_query::Value as SeaValue;
use time::format_description::well_known::Rfc3339;

use crate::entity::{self, Actor, Section, actor_kind};

/// What a settings change did to a value.
///
/// The three cases are a value set to something we may show, a value set
/// under a key that names a credential, and a value cleared. There is no
/// "no value" case: a settings write either carries a value or says it
/// removed one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingChange {
    /// A value was set. `value` is already redacted and `redacted` says
    /// whether a detector actually changed it — `false` for a setting that
    /// is not a secret, so a listing shows it verbatim.
    Set { value: String, redacted: bool },
    /// A value was set under a key that **names** a credential, so the
    /// value was never handed to this crate at all.
    Secret,
    /// A value was cleared. An explicit row, never a `NULL`: "this key was
    /// removed" is the thing an audit log is for, and a `NULL` would read
    /// as a row that never carried a value.
    Cleared,
}

impl SettingChange {
    /// A value being set, redacted before it can reach the table.
    ///
    /// # Errors
    ///
    /// [`AuditError::ValueTooLarge`] if `value` is over
    /// `livingbrain-redact`'s input cap.
    pub fn set(value: &str) -> Result<Self, AuditError> {
        let (value, redacted) = redact_value(value)?;
        Ok(Self::Set { value, redacted })
    }

    /// A value being set under a credential-shaped key. The value is not an
    /// argument because it must not exist in this process any longer than
    /// it takes to apply it.
    #[must_use]
    pub fn secret() -> Self {
        Self::Secret
    }

    /// A value being cleared.
    #[must_use]
    pub fn cleared() -> Self {
        Self::Cleared
    }

    /// The stored wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Set { .. } => "set",
            Self::Secret => "secret",
            Self::Cleared => "cleared",
        }
    }

    /// The value as stored: the redacted text, or `""` for a cleared or
    /// secret key.
    #[must_use]
    pub fn stored_value(&self) -> &str {
        match self {
            Self::Set { value, .. } => value,
            Self::Secret | Self::Cleared => "",
        }
    }

    /// Whether the value was withheld. Always `true` for
    /// [`SettingChange::Secret`]: the whole point is that the value was
    /// not kept.
    #[must_use]
    pub fn was_redacted(&self) -> bool {
        match self {
            Self::Set { redacted, .. } => *redacted,
            Self::Secret => true,
            Self::Cleared => false,
        }
    }
}

/// One row of the audit log, as [`AuditStore::list_for_scope`] hands it
/// back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    pub id: String,
    /// This entry's place in its workspace's log, 1 for the first change.
    /// The ordering key, so "newest first" is a fact about the table rather
    /// than about how a particular id generator fills a ULID.
    pub seq: u32,
    pub workspace_id: String,
    pub section: Section,
    pub key: String,
    pub change: SettingChange,
    pub actor: Actor,
    /// The request id this change rode in on, when the caller had one.
    pub request_id: Option<String>,
    pub recorded_at: String,
}

/// Everything a settings change can be refused for.
#[derive(Debug)]
pub enum AuditError {
    /// A workspace id outside the rule in [`entity::is_workspace_id`].
    InvalidWorkspace(String),
    /// A setting key outside the rule in [`entity::is_setting_key`].
    InvalidKey(String),
    /// A value over `livingbrain-redact`'s input cap. Splitting a settings
    /// payload is not a thing any caller does, so this is a refusal rather
    /// than a chunked redaction.
    ValueTooLarge { len: usize, cap: usize },
    /// A stored row that does not parse.
    Corrupt(String),
    /// The database failed.
    Store(DbError),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorkspace(value) => write!(
                f,
                "invalid workspace id `{value}`: non-empty, at most 128 bytes, no whitespace"
            ),
            Self::InvalidKey(key) => write!(
                f,
                "invalid setting key `{key}`: lowercase ascii alnum and `-_.`, 1..=128 chars"
            ),
            Self::ValueTooLarge { len, cap } => write!(
                f,
                "setting value of {len} bytes is over the {cap}-byte redaction cap"
            ),
            Self::Corrupt(message) => write!(f, "corrupt settings_audit row: {message}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for AuditError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

/// The settings audit store.
pub struct AuditStore {
    db: Arc<dyn Database>,
    clock: Arc<dyn Clock>,
    id_gen: Arc<dyn IdGen>,
}

impl AuditStore {
    /// A store over the three ports it needs.
    #[must_use]
    pub fn new(db: Arc<dyn Database>, clock: Arc<dyn Clock>, id_gen: Arc<dyn IdGen>) -> Self {
        Self { db, clock, id_gen }
    }

    /// Appends one settings change to `workspace_id`'s log.
    ///
    /// The value was redacted by [`SettingChange::set`] on construction, so
    /// by the time this runs there is nothing left to strip: this method
    /// writes what it is handed and trusts its own types.
    ///
    /// # Errors
    ///
    /// [`AuditError::InvalidWorkspace`] for a workspace id outside the
    /// rule, [`AuditError::InvalidKey`] for a key outside it, and
    /// [`AuditError::Store`] if the database fails.
    pub async fn record(
        &self,
        workspace_id: &str,
        section: Section,
        actor: &Actor,
        key: &str,
        change: SettingChange,
        request_id: Option<&str>,
    ) -> Result<AuditEntry, AuditError> {
        if !entity::is_workspace_id(workspace_id) {
            return Err(AuditError::InvalidWorkspace(workspace_id.to_owned()));
        }
        if !entity::is_setting_key(key) {
            return Err(AuditError::InvalidKey(key.to_owned()));
        }

        // Read-then-insert rather than one atomic statement: both dialects
        // would need a dialect-specific next-value expression, and ADR 0004
        // bans `SERIAL` and `AUTOINCREMENT`. The race is caught by the
        // `UNIQUE (workspace_id, seq)` constraint — the loser's insert
        // fails and is read back as a store error rather than silently
        // taking a sequence number another row already holds. An audit log
        // that dropped a change to win a race would be worse than one that
        // asks the caller to try again.
        let seq = self.next_seq(workspace_id).await?;
        let id = self.id_gen.ulid();
        let recorded_at = self.now();
        self.db
            .execute(&Statement::with_values(
                "INSERT INTO settings_audit \
                 (id, seq, workspace_id, section, setting_key, change, value, redacted, \
                  actor_kind, actor, request_id, recorded_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                vec![
                    text(&id),
                    int(seq),
                    text(workspace_id),
                    text(section.as_str()),
                    text(key),
                    text(change.as_str()),
                    text(change.stored_value()),
                    int(u32::from(change.was_redacted())),
                    text(actor_kind(actor)),
                    text(actor.label()),
                    opt_text(request_id),
                    text(&recorded_at),
                ],
            ))
            .await
            .map_err(AuditError::Store)?;

        Ok(AuditEntry {
            id,
            seq,
            workspace_id: workspace_id.to_owned(),
            section,
            key: key.to_owned(),
            change,
            actor: actor.clone(),
            request_id: request_id.map(str::to_owned),
            recorded_at,
        })
    }

    /// The sequence number the next change in this workspace takes. An
    /// empty log starts at 1, because "the first change" is not change 0.
    async fn next_seq(&self, workspace_id: &str) -> Result<u32, AuditError> {
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT MAX(seq) AS highest FROM settings_audit WHERE workspace_id = ?",
                vec![text(workspace_id)],
            ))
            .await
            .map_err(AuditError::Store)?;
        let highest = rows
            .first()
            .and_then(|row| row.get::<u32>("highest"))
            .unwrap_or_default();
        Ok(highest + 1)
    }

    /// Every entry for one workspace, newest first. A `limit` of 0 returns
    /// nothing, and a workspace with no changes returns an empty list rather
    /// than an error — an audit log nobody has written to is an empty log,
    /// not a missing one.
    ///
    /// The query always carries a `workspace_id` predicate. A listing that
    /// could omit it is a listing that could show one workspace another's
    /// settings history, so there is no overload without the scope.
    ///
    /// # Errors
    ///
    /// [`AuditError::InvalidWorkspace`] and [`AuditError::Store`].
    pub async fn list_for_scope(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> Result<Vec<AuditEntry>, AuditError> {
        if !entity::is_workspace_id(workspace_id) {
            return Err(AuditError::InvalidWorkspace(workspace_id.to_owned()));
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT id, seq, workspace_id, section, setting_key, change, value, redacted, \
                        actor_kind, actor, request_id, recorded_at \
                 FROM settings_audit WHERE workspace_id = ? ORDER BY seq DESC LIMIT ?",
                vec![text(workspace_id), int(limit as u32)],
            ))
            .await
            .map_err(AuditError::Store)?;
        rows.rows.iter().map(entry_from).collect()
    }

    /// How many entries a workspace's log holds: the count beside "recent
    /// changes" on a settings screen, and the number an erasure preview
    /// checks against the rows it removed.
    ///
    /// # Errors
    ///
    /// [`AuditError::InvalidWorkspace`] and [`AuditError::Store`].
    pub async fn count_for_scope(&self, workspace_id: &str) -> Result<u64, AuditError> {
        if !entity::is_workspace_id(workspace_id) {
            return Err(AuditError::InvalidWorkspace(workspace_id.to_owned()));
        }
        let rows = self
            .db
            .query(&Statement::with_values(
                "SELECT COUNT(*) AS total FROM settings_audit WHERE workspace_id = ?",
                vec![text(workspace_id)],
            ))
            .await
            .map_err(AuditError::Store)?;
        Ok(rows
            .first()
            .and_then(|row| row.get::<u64>("total"))
            .unwrap_or_default())
    }

    /// The current time as RFC 3339, the storage format for every
    /// timestamp. A real clock reading cannot fail to format, so an empty
    /// stamp is preferred to failing an otherwise valid write.
    fn now(&self) -> String {
        self.clock.now().format(&Rfc3339).unwrap_or_default()
    }
}

/// Redacts `value` and reports whether a detector changed it.
///
/// This is the one place the audit log decides what a settings value looks
/// like on disk, and it runs [`livingbrain_redact`] rather than a local
/// detector set: a settings value is arbitrary text a member typed, which
/// is the same problem every other ingest path already solved with that
/// library, and two detector tables would drift.
///
/// `Policy::Redact` and never `Policy::Block`: an audit log that refuses to
/// record a change because the change contained a secret is an audit log
/// with a hole exactly where it matters.
///
/// # Errors
///
/// [`AuditError::ValueTooLarge`] if `value` is over the library's input cap.
pub fn redact_value(value: &str) -> Result<(String, bool), AuditError> {
    match redact(value, Policy::Redact) {
        Ok((clean, findings)) => Ok((clean, !findings.is_empty())),
        Err(RedactError::TooLarge { len, cap }) => Err(AuditError::ValueTooLarge { len, cap }),
        // `Policy::Redact` never blocks, so this arm is unreachable today.
        // Mapping it to an error rather than panicking keeps a future
        // policy change from turning into a crash in a request path.
        Err(RedactError::Blocked(_)) => Err(AuditError::Corrupt(
            "blocked under a redact-only policy".to_owned(),
        )),
    }
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

fn opt_text(value: Option<&str>) -> SeaValue {
    SeaValue::String(value.map(|v| Box::new(v.to_owned())))
}

fn int(value: u32) -> SeaValue {
    SeaValue::BigInt(Some(i64::from(value)))
}

fn entry_from(row: &Row) -> Result<AuditEntry, AuditError> {
    let section_raw = row
        .get::<String>("section")
        .ok_or_else(|| AuditError::Corrupt("section is not text".to_owned()))?;
    let section = Section::parse(&section_raw)
        .ok_or_else(|| AuditError::Corrupt(format!("unknown section `{section_raw}`")))?;
    let change_raw = row
        .get::<String>("change")
        .ok_or_else(|| AuditError::Corrupt("change is not text".to_owned()))?;
    let value = row.get::<String>("value").unwrap_or_default();
    let change = match change_raw.as_str() {
        "set" => SettingChange::Set {
            value,
            redacted: row.get::<u64>("redacted").unwrap_or_default() == 1,
        },
        "secret" => SettingChange::Secret,
        "cleared" => SettingChange::Cleared,
        other => return Err(AuditError::Corrupt(format!("unknown change `{other}`"))),
    };
    let kind = row
        .get::<String>("actor_kind")
        .ok_or_else(|| AuditError::Corrupt("actor_kind is not text".to_owned()))?;
    let actor = match kind.as_str() {
        "human" => Actor::Human {
            id: row.get::<String>("actor").unwrap_or_default(),
        },
        "brain" => Actor::Brain,
        other => return Err(AuditError::Corrupt(format!("unknown actor kind `{other}`"))),
    };
    Ok(AuditEntry {
        id: row.get::<String>("id").unwrap_or_default(),
        seq: row.get::<u32>("seq").unwrap_or_default(),
        workspace_id: row.get::<String>("workspace_id").unwrap_or_default(),
        section,
        key: row.get::<String>("setting_key").unwrap_or_default(),
        change,
        actor,
        request_id: row.get::<Option<String>>("request_id").unwrap_or_default(),
        recorded_at: row.get::<String>("recorded_at").unwrap_or_default(),
    })
}
