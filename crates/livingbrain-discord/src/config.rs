//! Where the Discord app's credentials come from: three Worker secrets.
//!
//! `DISCORD_CLIENT_ID` (public, in the install URL), `DISCORD_CLIENT_SECRET`
//! (the OAuth secret, unused by the bot-only flow but kept so the install
//! callback can be verified later) and `DISCORD_BOT_TOKEN` (the bot's token;
//! every read and every confirmation is made with it).
//!
//! Unset, blank or a placeholder (see [`is_real_credential`], the same rule as
//! the Slack keys in `livingbrain-workspaces`) and the integration reports
//! itself unavailable: the settings page shows nothing, and every route
//! answers 503. Nothing here is ever printed.

use cratefield_core::Config;

use crate::common::Secret;

/// The bot's permissions: `VIEW_CHANNEL` (1 << 10) and `READ_MESSAGE_HISTORY`
/// (1 << 16). Nothing else. An admin who grants more has granted it on Discord.
pub const PERMISSIONS: u64 = (1 << 10) | (1 << 16);

/// The Discord app, when its three secrets are real values.
pub struct DiscordApp {
    pub(crate) client_id: String,
    pub(crate) bot_token: Secret,
}

impl std::fmt::Debug for DiscordApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordApp")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

fn read(cfg: &dyn Config, key: &str) -> Option<String> {
    cfg.get(key)
        .map(|raw| raw.trim().to_owned())
        .filter(|raw| is_real_credential(raw))
}

impl DiscordApp {
    /// The app, when all three secrets are set and real.
    #[must_use]
    pub fn from_config(cfg: &dyn Config) -> Option<Self> {
        let client_id = read(cfg, "DISCORD_CLIENT_ID")
            .filter(|id| id.len() <= 20 && id.bytes().all(|b| b.is_ascii_digit()))?;
        // Required, and checked, but not held: the bot-only flow never signs
        // in as the user, so nothing reads the OAuth secret yet.
        read(cfg, "DISCORD_CLIENT_SECRET")?;
        let bot_token =
            read(cfg, "DISCORD_BOT_TOKEN").filter(|t| !t.contains(char::is_whitespace))?;
        Some(Self {
            client_id,
            bot_token: Secret::new(bot_token),
        })
    }
}

/// Whether the integration can run on this deployment.
#[must_use]
pub fn is_available(cfg: &dyn Config) -> bool {
    DiscordApp::from_config(cfg).is_some()
}

/// Whether a credential is a real value rather than a placeholder: not blank,
/// not `xxx`, not a `replace-with-…` or `changeme`-style stand-in.
#[must_use]
pub fn is_real_credential(raw: &str) -> bool {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() {
        return false;
    }
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
        "your-discord",
    ]
    .iter()
    .all(|word| !value.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::MapConfig;

    fn full() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DISCORD_CLIENT_ID", "1234567890123456789"),
            ("DISCORD_CLIENT_SECRET", "discord-client-secret-value"),
            ("DISCORD_BOT_TOKEN", "MTIzNDU2.bot.token-value"),
        ]
    }

    #[test]
    fn all_three_real_secrets_make_it_available() {
        assert!(is_available(&MapConfig::from_pairs(full())));
    }

    #[test]
    fn any_missing_blank_or_placeholder_secret_makes_it_unavailable() {
        for key in [
            "DISCORD_CLIENT_ID",
            "DISCORD_CLIENT_SECRET",
            "DISCORD_BOT_TOKEN",
        ] {
            let without: Vec<_> = full().into_iter().filter(|(k, _)| *k != key).collect();
            assert!(
                !is_available(&MapConfig::from_pairs(without)),
                "{key} missing"
            );
            for bad in [
                "",
                "   ",
                "replace-with-your-discord-app-client-id",
                "changeme",
                "xxx",
            ] {
                let pairs: Vec<_> = full()
                    .into_iter()
                    .map(|(k, v)| if k == key { (k, bad) } else { (k, v) })
                    .collect();
                assert!(
                    !is_available(&MapConfig::from_pairs(pairs)),
                    "{key}={bad:?}"
                );
            }
        }
    }

    #[test]
    fn a_client_id_must_be_digits_and_a_token_must_have_no_spaces() {
        let pairs = |id: &'static str, token: &'static str| {
            full()
                .into_iter()
                .map(|(k, v)| match k {
                    "DISCORD_CLIENT_ID" => (k, id),
                    "DISCORD_BOT_TOKEN" => (k, token),
                    _ => (k, v),
                })
                .collect::<Vec<_>>()
        };
        assert!(!is_available(&MapConfig::from_pairs(pairs(
            "abc", "MTIz.x.y"
        ))));
        assert!(!is_available(&MapConfig::from_pairs(pairs(
            "123", "MTIz x.y"
        ))));
    }

    #[test]
    fn debug_never_prints_a_secret() {
        let app = DiscordApp::from_config(&MapConfig::from_pairs(full())).unwrap();
        let printed = format!("{app:?}");
        assert!(!printed.contains("bot"), "{printed}");
        assert!(!printed.contains("secret"), "{printed}");
    }
}
