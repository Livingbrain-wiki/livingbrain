//! Where the Slack app's credentials come from.
//!
//! Module `workspaces`, so `ModuleConfig` derives the env keys by prefix:
//! `WORKSPACES_SLACK_CLIENT_ID`, `WORKSPACES_SLACK_CLIENT_SECRET` and
//! `WORKSPACES_REDIRECT_BASE`.

use cratefield_core::{Config, ConfigError, ModuleConfig};

/// The Slack app settings the module needs, read once when the router is
/// built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// The Slack app's client id (`WORKSPACES_SLACK_CLIENT_ID`).
    pub client_id: String,
    /// The Slack app's client secret (`WORKSPACES_SLACK_CLIENT_SECRET`).
    ///
    /// A secret: locally it belongs in `crates/livingbrain-venture/.dev.vars`
    /// (gitignored), and in a deployment in a Worker secret. It never goes in
    /// `wrangler.toml` `[vars]`, and no error message below quotes it.
    pub client_secret: String,
    /// The public origin Slack redirects back to
    /// (`WORKSPACES_REDIRECT_BASE`), without a trailing slash. Under
    /// `wrangler dev` this is the tunnel URL, not `localhost`.
    pub redirect_base: String,
}

impl Settings {
    /// Reads the Slack app settings from harness config.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming every key that is missing or malformed, so
    /// `fz doctor` reports all of them at once. It never panics, and no
    /// message it produces contains a configured value — a client secret
    /// that failed to parse must not be echoed into a log.
    pub fn from_config(cfg: &dyn Config) -> Result<Self, ConfigError> {
        let module = ModuleConfig::new("workspaces", cfg);
        let mut errors = ConfigError::default();
        let client_id = required(
            cfg,
            &module,
            "SLACK_CLIENT_ID",
            "the Slack app's client id",
            &mut errors,
        );
        let client_secret = required(
            cfg,
            &module,
            "SLACK_CLIENT_SECRET",
            "the Slack app's client secret",
            &mut errors,
        );
        let redirect_base = required(
            cfg,
            &module,
            "REDIRECT_BASE",
            "the public origin Slack redirects back to, e.g. the tunnel URL under `wrangler dev`",
            &mut errors,
        );

        if let Some(base) = redirect_base.as_deref()
            && !(base.starts_with("https://") || base.starts_with("http://"))
        {
            errors.push(format!(
                "workspaces: {} must be an absolute http(s) origin, got {base:?}",
                module.key("REDIRECT_BASE")
            ));
        }
        errors.into_result()?;

        // `into_result` proved all three are `Some`.
        Ok(Self {
            client_id: client_id.unwrap_or_default(),
            client_secret: client_secret.unwrap_or_default(),
            redirect_base: redirect_base
                .unwrap_or_default()
                .trim_end_matches('/')
                .to_owned(),
        })
    }

    /// The redirect URI to register with the Slack app, and the one the
    /// authorize and token calls both carry. They must be byte-identical.
    #[must_use]
    pub fn callback_url(&self) -> String {
        format!("{}/v1/workspaces/slack/callback", self.redirect_base)
    }
}

/// One required key, recording a named error when it is absent or empty.
fn required(
    cfg: &dyn Config,
    module: &ModuleConfig<'_>,
    suffix: &str,
    what: &str,
    errors: &mut ConfigError,
) -> Option<String> {
    let value = cfg
        .get(&module.key(suffix))
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty());
    if value.is_none() {
        errors.push(format!(
            "workspaces: {} is required ({what})",
            module.key(suffix)
        ));
    }
    value
}
