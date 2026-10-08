//! What a settings change *is*: the section it belongs to, the key inside
//! it, and who made it. The values themselves never reach this file — they
//! are redacted by [`crate::SettingChange`] before the store sees them.

/// The longest a setting key may be, in bytes.
pub const MAX_KEY_LEN: usize = 128;

/// The settings sections of the web app (issue #30). A change names one, so
/// a listing can answer "what happened to the models settings" without
/// parsing keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Models,
    Proactivity,
    Tools,
    Automations,
    Skills,
    Colonizer,
}

impl Section {
    /// Every section, in the order the settings screen shows them.
    pub const ALL: &'static [Section] = &[
        Section::Models,
        Section::Proactivity,
        Section::Tools,
        Section::Automations,
        Section::Skills,
        Section::Colonizer,
    ];

    /// The stored wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Models => "models",
            Self::Proactivity => "proactivity",
            Self::Tools => "tools",
            Self::Automations => "automations",
            Self::Skills => "skills",
            Self::Colonizer => "colonizer",
        }
    }

    /// Parses a stored wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|section| section.as_str() == value)
    }
}

/// Who changed a setting. Mirrors `livingbrain-pages`' `Author` shape — the
/// actor is either a person by their id, or the brain — because the two
/// modules answer the same question and a reader should not have to learn
/// two vocabularies for "who did this".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actor {
    /// A person, by their user id.
    Human { id: String },
    /// The brain.
    Brain,
}

impl Actor {
    /// The stored author label: the human's id, or `brain`.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Human { id } => id,
            Self::Brain => "brain",
        }
    }
}

/// The stored wire name of an [`Actor`]'s kind.
#[must_use]
pub fn actor_kind(actor: &Actor) -> &'static str {
    match actor {
        Actor::Human { .. } => "human",
        Actor::Brain => "brain",
    }
}

/// The setting key rule: lowercase ascii letters, digits, `-`, `_` and `.`,
/// 1..=[`MAX_KEY_LEN`] bytes. Dots because a key may address a nested
/// setting (`model.default`); nothing looser, because the key is echoed
/// back in a listing and into an export.
#[must_use]
pub fn is_setting_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_LEN
        && value.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
        })
}

/// The workspace id rule. A workspace id is whatever the tenancy module
/// minted — a Slack team id (`T0…`), a Discord guild id, or one of our own
/// ULIDs since issue #71 — so this only refuses what no workspace id can
/// be: empty, over-long, or carrying whitespace or a control character.
#[must_use]
pub fn is_workspace_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_KEY_LEN
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_round_trip() {
        for section in Section::ALL {
            assert_eq!(Section::parse(section.as_str()), Some(*section));
        }
        assert_eq!(Section::parse("unknown"), None);
    }

    #[test]
    fn keys_follow_the_rule() {
        assert!(is_setting_key("daily"));
        assert!(is_setting_key("model.default"));
        assert!(is_setting_key("api-key_2"));
        assert!(!is_setting_key(""));
        assert!(!is_setting_key("Daily"));
        assert!(!is_setting_key("has space"));
        assert!(!is_setting_key(&"x".repeat(MAX_KEY_LEN + 1)));
    }

    #[test]
    fn workspace_ids_reject_only_what_cannot_be_one() {
        // A Slack team id is uppercase; our own ids since #71 are ULIDs.
        assert!(is_workspace_id("T0SPACE"));
        assert!(is_workspace_id("01HQ0000000000000000000000"));
        assert!(!is_workspace_id(""));
        assert!(!is_workspace_id("has space"));
        assert!(!is_workspace_id("has\nnewline"));
    }
}
