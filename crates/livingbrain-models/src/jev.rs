//! Jev (TypeSafe's System One), the coding-agents compiler's optional
//! triage classifier, per workspace.
//!
//! The classifier itself is the venture's: this module never names the
//! TypeSafe adapter. The venture hands it a [`JevConnector`] — "do you have
//! a deployment key, and build me a classifier over this key and model" —
//! and this module decides, per workspace, which key (if any) Jev runs on:
//!
//! 1. the workspace switched Jev off → **off**, whatever key exists (the
//!    compiler's free triage rules still run);
//! 2. the workspace stored its own TypeSafe key → **workspace**;
//! 3. the deployment has `TYPESAFE_API_KEY` → **deployment**;
//! 4. otherwise → **off**.
//!
//! That rule is [`decide`]; [`effective`] reads the row and applies it.
//!
//! The workspace key is stored exactly like a model connection's key:
//! AES-256-GCM under `MODEL_KEYS_SECRET`, bound by AAD to
//! `(workspace_id, "jev")`, so a ciphertext lifted from a model connection
//! row (whose AAD names a role) cannot open here, nor this one there. Only
//! its last four characters are ever shown; no route returns it and nothing
//! logs it.
//!
//! What Jev receives is the compiler's redacted session digest (at most
//! 24k characters), on TypeSafe's US servers; what comes back is a
//! classification, nothing more.

use std::sync::Arc;

use cratefield_core::{Classifier, Config, Database, DbError, Statement};
use sea_query::Value;

use crate::crypto;

/// The AAD "role" a workspace's TypeSafe key is bound to. Not one of the
/// model roles, so the two key spaces never open each other's ciphertext.
pub(crate) const AAD_ROLE: &str = "jev";

/// Which key a classifier is built over.
#[derive(Clone, Copy)]
pub enum JevKey<'a> {
    /// The deployment's own `TYPESAFE_API_KEY`.
    Deployment,
    /// A workspace's own key, decrypted for this one classifier.
    Workspace(&'a str),
}

impl std::fmt::Debug for JevKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Deployment => f.write_str("Deployment"),
            // Never the key: a debug line is a log line.
            Self::Workspace(_) => f.write_str("Workspace(…)"),
        }
    }
}

/// The venture's half: what the deployment has, and how a classifier is
/// built. The module asks nothing else of TypeSafe.
pub trait JevConnector: Send + Sync {
    /// Whether the deployment carries its own `TYPESAFE_API_KEY`.
    fn deployment_key(&self) -> bool;

    /// The model a workspace that names none runs: the deployment's
    /// `TYPESAFE_MODEL`, else the adapter's pinned default.
    fn default_model(&self) -> String;

    /// A classifier over `key` and `model`, or `None` when it cannot be
    /// built (no deployment key for [`JevKey::Deployment`]).
    fn classifier(&self, key: JevKey<'_>, model: &str) -> Option<Arc<dyn Classifier>>;
}

/// A connector over one fixed classifier, for a composition (or a test)
/// that already holds one: it stands in for the deployment key, whatever
/// key or model is asked for.
pub struct FixedJev {
    classifier: Arc<dyn Classifier>,
    model: String,
}

impl FixedJev {
    /// A connector that always answers `classifier`.
    #[must_use]
    pub fn new(classifier: Arc<dyn Classifier>, model: impl Into<String>) -> Self {
        Self {
            classifier,
            model: model.into(),
        }
    }
}

impl JevConnector for FixedJev {
    fn deployment_key(&self) -> bool {
        true
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }

    fn classifier(&self, _key: JevKey<'_>, _model: &str) -> Option<Arc<dyn Classifier>> {
        Some(self.classifier.clone())
    }
}

/// Where a workspace's Jev runs from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// No classifier: switched off, or no key anywhere.
    Off,
    /// The deployment's `TYPESAFE_API_KEY`.
    Deployment,
    /// The workspace's own key.
    Workspace,
}

impl Source {
    /// The wire name: `off`, `deployment` or `workspace`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Deployment => "deployment",
            Self::Workspace => "workspace",
        }
    }
}

/// The precedence rule: the workspace's switch first, then its own key,
/// then the deployment's, else off. `enabled` is `None` when the workspace
/// never touched the switch (on by default).
#[must_use]
pub fn decide(enabled: Option<bool>, workspace_key: bool, deployment_key: bool) -> Source {
    if enabled == Some(false) {
        Source::Off
    } else if workspace_key {
        Source::Workspace
    } else if deployment_key {
        Source::Deployment
    } else {
        Source::Off
    }
}

/// A workspace's stored Jev settings, minus the ciphertext.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stored {
    pub(crate) enabled: Option<bool>,
    pub(crate) model: Option<String>,
    pub(crate) key_last4: Option<String>,
    pub(crate) has_key: bool,
    pub(crate) updated_at: Option<String>,
}

/// The classifier a workspace's compile runs on, and where it came from.
pub struct Effective {
    pub source: Source,
    /// The model Jev runs (the workspace's choice, else the default).
    pub model: String,
    /// `None` exactly when `source` is [`Source::Off`].
    pub classifier: Option<Arc<dyn Classifier>>,
}

impl std::fmt::Debug for Effective {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effective")
            .field("source", &self.source)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

/// The classifier `workspace`'s compiles run on: [`decide`] over its row,
/// with its own key decrypted for this one classifier. A stored key that
/// no longer opens (the secret rotated) counts as no workspace key, so the
/// deployment's still serves.
///
/// # Errors
///
/// The database's, reading the row.
pub async fn effective(
    db: &dyn Database,
    config: &dyn Config,
    workspace: &str,
    connector: &dyn JevConnector,
) -> Result<Effective, DbError> {
    let stored = read(db, workspace).await?.unwrap_or_default();
    let model = stored
        .model
        .clone()
        .unwrap_or_else(|| connector.default_model());
    let key = if stored.has_key && stored.enabled != Some(false) {
        open_key(db, config, workspace).await?
    } else {
        None
    };
    let source = decide(stored.enabled, key.is_some(), connector.deployment_key());
    let classifier = match source {
        Source::Off => None,
        Source::Workspace => key
            .as_deref()
            .and_then(|key| connector.classifier(JevKey::Workspace(key), &model)),
        Source::Deployment => connector.classifier(JevKey::Deployment, &model),
    };
    let source = if classifier.is_some() {
        source
    } else {
        Source::Off
    };
    Ok(Effective {
        source,
        model,
        classifier,
    })
}

/// The workspace's own key, decrypted, or `None` (none stored, no secret,
/// or a ciphertext that no longer opens).
pub(crate) async fn open_key(
    db: &dyn Database,
    config: &dyn Config,
    workspace: &str,
) -> Result<Option<String>, DbError> {
    let Some(secret) = crypto::derive_key(config) else {
        return Ok(None);
    };
    let Some(blob) = ciphertext(db, workspace).await? else {
        return Ok(None);
    };
    Ok(crypto::decrypt(&secret, &blob, workspace, AAD_ROLE)
        .ok()
        .and_then(|key| String::from_utf8(key).ok()))
}

/// The row, without the ciphertext column.
pub(crate) async fn read(db: &dyn Database, workspace: &str) -> Result<Option<Stored>, DbError> {
    let statement = Statement::with_values(
        "SELECT enabled, model, key_last4, \
                CASE WHEN key_ciphertext IS NULL THEN 0 ELSE 1 END AS has_key, updated_at \
         FROM jev_settings WHERE workspace_id = ?",
        vec![text(workspace)],
    );
    Ok(db.query(&statement).await?.first().map(|row| Stored {
        enabled: Some(row.get::<i64>("enabled").unwrap_or(1) != 0),
        model: row
            .get::<Option<String>>("model")
            .flatten()
            .filter(|model| !model.is_empty()),
        key_last4: row.get::<Option<String>>("key_last4").flatten(),
        has_key: row.get::<i64>("has_key").unwrap_or_default() != 0,
        updated_at: row.get::<Option<String>>("updated_at").flatten(),
    }))
}

/// The only read of the ciphertext column.
async fn ciphertext(db: &dyn Database, workspace: &str) -> Result<Option<Vec<u8>>, DbError> {
    let statement = Statement::with_values(
        "SELECT key_ciphertext FROM jev_settings WHERE workspace_id = ?",
        vec![text(workspace)],
    );
    Ok(db
        .query(&statement)
        .await?
        .first()
        .and_then(|row| row.get::<Vec<u8>>("key_ciphertext")))
}

/// What one write changes; `None` leaves a field as it is.
#[derive(Default)]
pub(crate) struct Change<'a> {
    pub(crate) enabled: Option<bool>,
    /// `Some(None)` clears the model back to the default.
    pub(crate) model: Option<Option<&'a str>>,
    /// `Some(None)` removes the workspace key.
    pub(crate) key: Option<Option<(&'a [u8], &'a str)>>,
}

/// Applies `change` to the workspace's row, creating it with the defaults
/// first when there is none.
pub(crate) async fn write(
    db: &dyn Database,
    workspace: &str,
    change: Change<'_>,
    updated_by: &str,
    now: &str,
) -> Result<(), DbError> {
    db.execute(&Statement::with_values(
        "INSERT INTO jev_settings (workspace_id, enabled, updated_by, updated_at) \
         VALUES (?, 1, ?, ?) ON CONFLICT (workspace_id) DO NOTHING",
        vec![text(workspace), text(updated_by), text(now)],
    ))
    .await?;
    if let Some(enabled) = change.enabled {
        db.execute(&Statement::with_values(
            "UPDATE jev_settings SET enabled = ? WHERE workspace_id = ?",
            vec![Value::BigInt(Some(i64::from(enabled))), text(workspace)],
        ))
        .await?;
    }
    if let Some(model) = change.model {
        db.execute(&Statement::with_values(
            "UPDATE jev_settings SET model = ? WHERE workspace_id = ?",
            vec![model.map_or(Value::String(None), text), text(workspace)],
        ))
        .await?;
    }
    if let Some(key) = change.key {
        let (blob, last4) = match key {
            Some((blob, last4)) => (Value::Bytes(Some(Box::new(blob.to_vec()))), text(last4)),
            None => (Value::Bytes(None), Value::String(None)),
        };
        db.execute(&Statement::with_values(
            "UPDATE jev_settings SET key_ciphertext = ?, key_last4 = ? WHERE workspace_id = ?",
            vec![blob, last4, text(workspace)],
        ))
        .await?;
    }
    db.execute(&Statement::with_values(
        "UPDATE jev_settings SET updated_by = ?, updated_at = ? WHERE workspace_id = ?",
        vec![text(updated_by), text(now), text(workspace)],
    ))
    .await
    .map(|_| ())
}

fn text(value: &str) -> Value {
    Value::String(Some(Box::new(value.to_owned())))
}

#[cfg(test)]
mod tests {
    use super::{Source, decide};

    #[test]
    fn the_workspace_switch_wins_over_every_key() {
        assert_eq!(decide(Some(false), true, true), Source::Off);
        assert_eq!(decide(Some(false), false, true), Source::Off);
    }

    #[test]
    fn a_workspace_key_wins_over_the_deployment_key() {
        assert_eq!(decide(None, true, true), Source::Workspace);
        assert_eq!(decide(Some(true), true, false), Source::Workspace);
    }

    #[test]
    fn the_deployment_key_serves_when_the_workspace_has_none() {
        assert_eq!(decide(None, false, true), Source::Deployment);
        assert_eq!(decide(Some(true), false, true), Source::Deployment);
    }

    #[test]
    fn no_key_anywhere_is_off() {
        assert_eq!(decide(None, false, false), Source::Off);
        assert_eq!(decide(Some(true), false, false), Source::Off);
    }
}
