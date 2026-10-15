//! The shared "Living Brain" GitHub App: one operator-owned app that every
//! workspace installs, beside the per-workspace apps of the manifest flow.
//!
//! Its credentials are **Worker secrets**, never rows: the operator creates
//! the app once (`scripts/github-shared-app.py`, docs/github.md "The shared
//! app") and pipes each value into `wrangler secret put`. They are read per
//! request from the config port, held in [`Secret`]s, and never returned,
//! logged or stored.
//!
//! All six keys must be set for shared mode to be on. Any one missing or
//! blank and [`SharedApp::from_config`] answers `None`: the settings page
//! says GitHub is not configured on this deployment, the shared install
//! route answers 503, and a delivery naming the shared app id is treated as
//! a delivery for an unknown app (401). The per-workspace flow is unaffected.

use cratefield_core::Config;

use crate::seal::Secret;

/// The Worker secrets the shared app reads, in the order the setup script
/// sets them. Names only; never a value.
pub const SHARED_APP_KEYS: [&str; 6] = [
    "GITHUB_APP_ID",
    "GITHUB_APP_SLUG",
    "GITHUB_APP_CLIENT_ID",
    "GITHUB_APP_PRIVATE_KEY",
    "GITHUB_WEBHOOK_SECRET",
    "GITHUB_APP_CLIENT_SECRET",
];

/// The shared app, as this deployment's secrets describe it.
///
/// `client_id` and `client_secret` are for the one OAuth hop of the install
/// flow (proving who installed it); the private key signs the app JWT and
/// mints installation tokens; the webhook secret verifies deliveries.
pub struct SharedApp {
    pub app_id: i64,
    pub slug: String,
    pub client_id: String,
    pub private_key_pem: Secret,
    pub webhook_secret: Secret,
    pub client_secret: Secret,
}

impl std::fmt::Debug for SharedApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedApp")
            .field("app_id", &self.app_id)
            .field("slug", &self.slug)
            .finish_non_exhaustive()
    }
}

fn read(cfg: &dyn Config, key: &str) -> Option<String> {
    cfg.get(key)
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| !raw.is_empty())
}

/// A GitHub App slug: lowercase alphanumerics and hyphens.
pub(crate) fn is_slug(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// An OAuth client id as GitHub mints them (`Iv1.…`, `Iv23…`).
fn is_client_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.')
}

impl SharedApp {
    /// The shared app, when every one of [`SHARED_APP_KEYS`] is set and
    /// well formed; `None` otherwise (shared mode off).
    #[must_use]
    pub fn from_config(cfg: &dyn Config) -> Option<Self> {
        let app_id = read(cfg, "GITHUB_APP_ID")?
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)?;
        let slug = read(cfg, "GITHUB_APP_SLUG").filter(|slug| is_slug(slug))?;
        let client_id = read(cfg, "GITHUB_APP_CLIENT_ID").filter(|id| is_client_id(id))?;
        let pem = read(cfg, "GITHUB_APP_PRIVATE_KEY")?;
        // A PEM pasted as one line with literal `\n`s is still the PEM.
        let pem = if pem.contains('\n') {
            pem
        } else {
            pem.replace("\\n", "\n")
        };
        if !pem.starts_with("-----BEGIN") {
            return None;
        }
        let webhook_secret = read(cfg, "GITHUB_WEBHOOK_SECRET")?;
        let client_secret = read(cfg, "GITHUB_APP_CLIENT_SECRET")?;
        Some(Self {
            app_id,
            slug,
            client_id,
            private_key_pem: Secret::new(pem),
            webhook_secret: Secret::new(webhook_secret),
            client_secret: Secret::new(client_secret),
        })
    }

    /// Whether shared mode is on, without opening anything for long.
    #[must_use]
    pub fn configured(cfg: &dyn Config) -> bool {
        Self::from_config(cfg).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::MapConfig;

    fn full() -> Vec<(&'static str, &'static str)> {
        vec![
            ("GITHUB_APP_ID", "424242"),
            ("GITHUB_APP_SLUG", "living-brain"),
            ("GITHUB_APP_CLIENT_ID", "Iv23liSHARED"),
            (
                "GITHUB_APP_PRIVATE_KEY",
                "-----BEGIN RSA PRIVATE KEY-----\\nAAAA\\n-----END RSA PRIVATE KEY-----",
            ),
            ("GITHUB_WEBHOOK_SECRET", "shared-hook-secret"),
            ("GITHUB_APP_CLIENT_SECRET", "shared-client-secret"),
        ]
    }

    #[test]
    fn every_key_is_needed_and_a_one_line_pem_is_unfolded() {
        let app = SharedApp::from_config(&MapConfig::from_pairs(full())).expect("configured");
        assert_eq!(app.app_id, 424_242);
        assert!(app.private_key_pem.expose().contains("\nAAAA\n"));
        for key in SHARED_APP_KEYS {
            let pairs: Vec<_> = full().into_iter().filter(|(k, _)| *k != key).collect();
            assert!(
                SharedApp::from_config(&MapConfig::from_pairs(pairs)).is_none(),
                "{key} missing must turn shared mode off"
            );
            let blank: Vec<_> = full()
                .into_iter()
                .map(|(k, v)| if k == key { (k, "  ") } else { (k, v) })
                .collect();
            assert!(SharedApp::from_config(&MapConfig::from_pairs(blank)).is_none());
        }
    }

    #[test]
    fn a_malformed_id_slug_or_key_turns_shared_mode_off() {
        for (key, bad) in [
            ("GITHUB_APP_ID", "0"),
            ("GITHUB_APP_ID", "x"),
            ("GITHUB_APP_SLUG", "Living Brain"),
            ("GITHUB_APP_CLIENT_ID", "a/b"),
            ("GITHUB_APP_PRIVATE_KEY", "not a pem"),
        ] {
            let pairs: Vec<_> = full()
                .into_iter()
                .map(|(k, v)| if k == key { (k, bad) } else { (k, v) })
                .collect();
            assert!(SharedApp::from_config(&MapConfig::from_pairs(pairs)).is_none());
        }
    }

    #[test]
    fn debug_never_prints_a_credential() {
        let app = SharedApp::from_config(&MapConfig::from_pairs(full())).unwrap();
        let printed = format!("{app:?}");
        assert!(!printed.contains("secret"), "{printed}");
        assert!(!printed.contains("BEGIN"), "{printed}");
    }
}
