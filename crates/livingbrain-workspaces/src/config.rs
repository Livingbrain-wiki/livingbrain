//! Where the Slack app's credentials come from.
//!
//! Module `workspaces`, so `ModuleConfig` derives the env keys by prefix:
//! `WORKSPACES_SLACK_CLIENT_ID`, `WORKSPACES_SLACK_CLIENT_SECRET` and
//! `WORKSPACES_REDIRECT_BASE`.
//!
//! Since ADR 0002 Slack is one sign-in method among two, so [`Settings`] is
//! only the *Slack* half: the public origin and the sign-in sender are read
//! separately, by functions that a deployment with no Slack app can still
//! call.

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

    /// True when both Slack credentials are real values, not placeholders
    /// (see [`is_real_credential`]). This is what decides whether the sign-in
    /// page offers Slack.
    #[must_use]
    pub fn has_real_credentials(&self) -> bool {
        is_real_credential(&self.client_id) && is_real_credential(&self.client_secret)
    }
}

/// Whether a Slack credential is a real value rather than a placeholder.
///
/// `wrangler.toml` ships `replace-with-your-slack-app-client-id` until an
/// operator sets the real client id, so a value that is blank, or that reads
/// as a placeholder, is not configured. The check is deliberately blunt: a
/// real Slack client id is `digits.digits`, and a real secret is a long
/// random string, so none of the patterns below can match one by accident.
/// Matching is case-insensitive and ignores surrounding whitespace.
#[must_use]
pub fn is_real_credential(raw: &str) -> bool {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return false;
    }
    // `xxx`, `xxxx`, … are the usual "fill this in" stand-in.
    if value.len() >= 3 && value.chars().all(|c| c == 'x') {
        return false;
    }
    if value.starts_with("todo") {
        return false;
    }
    [
        "placeholder",
        "changeme",
        "change-me",
        "replace-with",
        "your-slack",
    ]
    .iter()
    .all(|word| !value.contains(word))
}

/// The Slack app's signing secret (`WORKSPACES_SLACK_SIGNING_SECRET`), or
/// [`None`] when it is unset or blank.
///
/// Read outside [`Settings`] and read *lazily*, per request, because it is
/// the one Slack credential an Events API app has that an OpenID Connect
/// sign-in app does not: a deployment may have both client keys set — signing
/// people in works fine — and no Events subscription at all, and requiring it
/// in [`Settings`] would fail `validate_config` for it. So
/// `POST /slack/events` answers `503` when it is absent, exactly as the
/// sign-in routes answer `503` without their keys.
///
/// A secret: it belongs in a Worker secret, never in `wrangler.toml`
/// `[vars]`, and no error message quotes it.
#[must_use]
pub fn signing_secret(cfg: &dyn Config) -> Option<String> {
    ModuleConfig::new("workspaces", cfg)
        .get_opt("SLACK_SIGNING_SECRET")
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
}

/// The sender a sign-in link is sent from when `WORKSPACES_MAIL_FROM` is
/// unset. It is a person-readable name on the venture's own domain, which
/// the mail provider must be configured to send for.
pub const DEFAULT_MAIL_FROM: &str = "Living Brain <no-reply@livingbrain.wiki>";

/// The sender for the sign-in mail (`WORKSPACES_MAIL_FROM`), or
/// [`DEFAULT_MAIL_FROM`].
///
/// Read outside [`Settings`] on purpose: a deployment with no Slack app
/// still sends sign-in mail, and `Settings` cannot be built without one.
/// A blank value reads as unset, so an empty secret in a `.dev.vars` cannot
/// become an empty sender.
#[must_use]
pub fn mail_from(cfg: &dyn Config) -> String {
    let configured = ModuleConfig::new("workspaces", cfg)
        .get_opt("MAIL_FROM")
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty());
    configured.unwrap_or_else(|| DEFAULT_MAIL_FROM.to_owned())
}

/// The public origin a sign-in link points at (`WORKSPACES_REDIRECT_BASE`),
/// without a trailing slash, or [`None`] when it is unset or is not an
/// absolute http(s) origin.
///
/// Same key as [`Settings::redirect_base`] and read the same way, but
/// without the two Slack credentials: a magic link is an absolute URL, so a
/// deployment that signs people in by email alone still needs the origin —
/// and must be refused one rather than mail a relative link nobody can
/// click. A configured-but-malformed value is a configuration error, so it
/// is reported as absent here and in [`Settings::from_config`].
#[must_use]
pub fn public_base(cfg: &dyn Config) -> Option<String> {
    let base = ModuleConfig::new("workspaces", cfg)
        .get_opt("REDIRECT_BASE")
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())?;
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return None;
    }
    Some(base.trim_end_matches('/').to_owned())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_slack_credentials_are_configured() {
        assert!(is_real_credential("1234.5678"));
        assert!(is_real_credential("8f3c9a0e1b7d4c2a9e6f5b4a3c2d1e0f"));
        // A real secret that happens to contain `x` or `todo` mid-value.
        assert!(is_real_credential("a1xxb2c3"));
        assert!(is_real_credential("mytodolist9"));
    }

    #[test]
    fn blank_and_placeholder_credentials_are_not_configured() {
        for placeholder in [
            "",
            "   ",
            "replace-with-your-slack-app-client-id",
            "REPLACE-WITH-YOUR-SLACK-APP-CLIENT-ID",
            "placeholder",
            "changeme",
            "change-me",
            "xxx",
            "XXXXXXXX",
            "TODO: set the client id",
            "your-slack-client-id",
        ] {
            assert!(!is_real_credential(placeholder), "{placeholder:?}");
        }
    }

    #[test]
    fn a_settings_pair_needs_both_credentials_real() {
        let settings = |id: &str, secret: &str| Settings {
            client_id: id.to_owned(),
            client_secret: secret.to_owned(),
            redirect_base: "https://brain.example".to_owned(),
        };
        assert!(settings("1234.5678", "8f3c9a0e1b7d4c2a").has_real_credentials());
        assert!(
            !settings("replace-with-your-slack-app-client-id", "8f3c9a0e1b7d4c2a")
                .has_real_credentials()
        );
        assert!(!settings("1234.5678", "changeme").has_real_credentials());
    }
}
