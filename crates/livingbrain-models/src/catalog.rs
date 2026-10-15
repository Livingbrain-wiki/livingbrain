//! The provider catalog: every LLM provider a model connection may name.
//!
//! The data is `app/assets/providers.json`, vendored from Colonizer's
//! `web/src/providerCatalog.ts` by `scripts/sync-providers` and compiled in
//! here with `include_str!`, so the server and the settings page read the
//! same file and cannot disagree about what a provider id means. Each entry
//! carries its base URL (possibly with `${VAR}` placeholders), the auth style
//! (`bearer` or `x-api-key`) and the wire (`anthropic` Messages or `openai`
//! chat completions).
//!
//! A provider id that is not in the catalog is refused, except `custom`,
//! which names its own base URL, wire and auth.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Deserialize;
use url::Url;

/// The vendored catalog. The path is the one file the app also imports.
const CATALOG_JSON: &str = include_str!("../../../app/assets/providers.json");

/// How a request carries the key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum Auth {
    /// `Authorization: Bearer <key>`.
    #[serde(rename = "bearer")]
    Bearer,
    /// `x-api-key: <key>`, Anthropic's header.
    #[serde(rename = "x-api-key")]
    XApiKey,
}

impl Auth {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::XApiKey => "x-api-key",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "bearer" => Some(Self::Bearer),
            "x-api-key" => Some(Self::XApiKey),
            _ => None,
        }
    }
}

/// Which API a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub(crate) enum Wire {
    /// `POST /v1/messages`.
    #[serde(rename = "anthropic")]
    Anthropic,
    /// `POST /v1/chat/completions`.
    #[serde(rename = "openai")]
    Openai,
}

impl Wire {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::Openai => "openai",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::Openai),
            _ => None,
        }
    }
}

/// A placeholder in a base URL, asked for when the provider is chosen.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Variable {
    pub(crate) name: String,
}

/// One provider.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) base_url: String,
    pub(crate) auth: Auth,
    pub(crate) wire: Wire,
    #[serde(default)]
    pub(crate) variables: Vec<Variable>,
}

#[derive(Debug, Deserialize)]
struct File {
    builtin: Vec<Entry>,
    providers: Vec<Entry>,
}

/// Every entry, built-ins first. Parsed once; a malformed file is a build
/// that cannot serve a connect, so the unit tests below parse it too.
fn entries() -> &'static [Entry] {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| match serde_json::from_str::<File>(CATALOG_JSON) {
        Ok(file) => file.builtin.into_iter().chain(file.providers).collect(),
        Err(_) => Vec::new(),
    })
}

/// The catalog entry for `id`, or `None`.
pub(crate) fn find(id: &str) -> Option<&'static Entry> {
    entries().iter().find(|entry| entry.id == id)
}

/// A variable value is a path or host fragment, so it is held to characters
/// that cannot change the URL's structure.
fn safe_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~'))
}

/// The entry's base URL with every `${VAR}` filled from `values`.
///
/// # Errors
///
/// A sentence naming the variable that is missing or not a plain value.
pub(crate) fn fill(entry: &Entry, values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut url = entry.base_url.clone();
    for variable in &entry.variables {
        let value = values
            .get(&variable.name)
            .map(|v| v.trim())
            .unwrap_or_default();
        if !safe_value(value) {
            return Err(format!(
                "{} must be letters, digits, '-', '_', '.' or '~'",
                variable.name
            ));
        }
        url = url.replace(&format!("${{{}}}", variable.name), value);
    }
    Ok(url)
}

/// `base` with an API path appended, the way Colonizer's gateway joins
/// them (`upstream_url`, issue #1018 there): a base whose last path segment
/// is a version (`/v1`, `/v3`) is the API root as its provider documents
/// it, so a `v1` at the head of `segments` is dropped instead of doubling
/// the version. Segments are pushed percent-encoded, so a model name cannot
/// walk out of its path.
pub(crate) fn join(base: &Url, segments: &[&str]) -> Option<Url> {
    let mut url = base.clone();
    let versioned = url
        .path_segments()
        .and_then(|mut parts| parts.rfind(|part| !part.is_empty()))
        .is_some_and(|last| {
            last.len() > 1 && last.starts_with('v') && last[1..].chars().all(|c| c.is_ascii_digit())
        });
    let segments = match segments.split_first() {
        Some((&"v1", rest)) if versioned => rest,
        _ => segments,
    };
    {
        let mut path = url.path_segments_mut().ok()?;
        path.pop_if_empty();
        for segment in segments {
            path.push(segment);
        }
    }
    Some(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vendored file parses, and every entry is usable: an https base
    /// URL, placeholders that match the variables, and unique ids. This is
    /// the server's half of the malformed-file check; `app/tests` holds the
    /// app's.
    #[test]
    fn the_vendored_catalog_is_well_formed() {
        let file: File = serde_json::from_str(CATALOG_JSON).expect("providers.json parses");
        assert!(file.providers.len() > 50, "{}", file.providers.len());
        let all: Vec<&Entry> = file.builtin.iter().chain(&file.providers).collect();
        let mut ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate ids");
        assert!(!ids.contains(&"custom"), "custom is not a catalog id");
        for entry in all {
            let values: BTreeMap<String, String> = entry
                .variables
                .iter()
                .map(|v| (v.name.clone(), "ep-1".to_owned()))
                .collect();
            let filled = fill(entry, &values).expect(&entry.id);
            assert!(!filled.contains("${"), "{} left a placeholder", entry.id);
            let url = Url::parse(&filled).expect(&entry.id);
            assert_eq!(url.scheme(), "https", "{}", entry.id);
        }
        assert_eq!(entries().len(), file.builtin.len() + file.providers.len());
    }

    #[test]
    fn the_built_ins_and_both_wires_are_present() {
        let anthropic = find("anthropic").expect("anthropic");
        assert_eq!(
            (anthropic.wire, anthropic.auth),
            (Wire::Anthropic, Auth::XApiKey)
        );
        let openai = find("openai").expect("openai");
        assert_eq!((openai.wire, openai.auth), (Wire::Openai, Auth::Bearer));
        assert!(find("xai-grok").is_some_and(|e| e.wire == Wire::Openai));
        assert!(find("no-such-provider").is_none());
    }

    #[test]
    fn variables_are_filled_and_held_to_plain_values() {
        let kat = find("kat-coder").expect("an entry with a variable");
        let mut values = BTreeMap::new();
        assert!(fill(kat, &values).is_err(), "missing");
        values.insert("ENDPOINT_ID".to_owned(), "ep-abc-123".to_owned());
        assert!(
            fill(kat, &values)
                .expect("filled")
                .contains("/endpoints/ep-abc-123/")
        );
        for bad in ["../x", "a/b", "a?b", "a#b", "a@b", ""] {
            values.insert("ENDPOINT_ID".to_owned(), bad.to_owned());
            assert!(fill(kat, &values).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_versioned_base_does_not_double_the_version() {
        let j = |base: &str, segments: &[&str]| {
            join(&Url::parse(base).expect("url"), segments)
                .expect("joins")
                .to_string()
        };
        assert_eq!(
            j("https://api.anthropic.com", &["v1", "messages"]),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            j("https://api.x.ai/v1", &["v1", "chat", "completions"]),
            "https://api.x.ai/v1/chat/completions"
        );
        assert_eq!(
            j(
                "https://models.example.com/v1/",
                &["v1", "chat", "completions"]
            ),
            "https://models.example.com/v1/chat/completions"
        );
        assert_eq!(
            j("https://ark.example.com/api/v3", &["v1", "models"]),
            "https://ark.example.com/api/v3/models"
        );
        assert_eq!(
            j("https://api.deepseek.com/anthropic", &["v1", "messages"]),
            "https://api.deepseek.com/anthropic/v1/messages"
        );
        assert_eq!(
            j("https://h.example/v1", &["v1", "models", "a/b"]),
            "https://h.example/v1/models/a%2Fb"
        );
    }
}
