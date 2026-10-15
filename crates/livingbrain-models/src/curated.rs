//! The curated model lists: what a provider is known to serve when it
//! publishes no list of its own.
//!
//! The data is `app/assets/model-cards.json`, Living Brain's own file (the
//! settings page's provider cards read the same one), compiled in with
//! `include_str!` like the catalog. A card names its catalog provider, how
//! that provider lists its models (`listing`), and the models it is known
//! to serve, with the context window and price the provider documents.
//! A catalog provider without a card falls back to the `models` the
//! vendored catalog itself names, if any.
//!
//! A `runner` card (a subscription run through the member's own CLI) has no
//! provider here: nothing about it is called from the server.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// The card file. The path is the one the app also imports.
const CARDS_JSON: &str = include_str!("../../../app/assets/model-cards.json");

/// One model a provider is known to serve.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Model {
    pub(crate) id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<String>,
    /// The context window, in tokens, as the provider documents it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) context: Option<u64>,
    /// Per million input / output tokens, as the provider documents it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) price: Option<String>,
}

impl Model {
    /// A bare id: a model a list named and nothing curated describes.
    pub(crate) fn bare(id: String) -> Self {
        Self {
            id,
            label: None,
            context: None,
            price: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CodingPlan {
    #[serde(default)]
    models: Vec<Model>,
}

#[derive(Debug, Deserialize)]
struct Card {
    kind: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    listing: Option<String>,
    #[serde(default)]
    models: Vec<Model>,
    #[serde(default)]
    coding_plan: Option<CodingPlan>,
    /// Whether the provider streams a completion (`"stream": true`). Absent
    /// is yes: both wires define streaming, and every provider carded today
    /// serves it. `false` sends that provider's calls unstreamed.
    #[serde(default)]
    stream: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct File {
    cards: Vec<Card>,
}

/// How a provider lists its models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Listing {
    /// `GET {base}/v1/models` on the connection's own base URL.
    Native,
    /// This OpenAI-compatible list URL, signed like the provider's calls.
    Url(String),
    /// The provider publishes no list: the curated models are the answer.
    None,
}

fn cards() -> &'static [Card] {
    static CARDS: OnceLock<Vec<Card>> = OnceLock::new();
    CARDS.get_or_init(|| {
        serde_json::from_str::<File>(CARDS_JSON)
            .map(|file| file.cards)
            .unwrap_or_default()
    })
}

/// The API-key card for a catalog provider, if there is one.
fn card(provider: &str) -> Option<&'static Card> {
    cards()
        .iter()
        .find(|card| card.kind != "runner" && card.provider.as_deref() == Some(provider))
}

/// How `provider` lists its models. A provider without a card is asked at
/// its own base URL, and a refusal is read as "no list" (see the probe).
pub(crate) fn listing(provider: &str) -> Listing {
    match card(provider).and_then(|card| card.listing.as_deref()) {
        Some("none") => Listing::None,
        Some(url) if url.starts_with("https://") => Listing::Url(url.to_owned()),
        _ => Listing::Native,
    }
}

/// Whether a model call to `provider` asks to stream: its card's `stream`
/// flag, else yes (a provider without a card — `custom` included — is
/// asked to; one that ignores the flag answers JSON, which is read too).
pub(crate) fn streams(provider: &str) -> bool {
    card(provider).and_then(|card| card.stream).unwrap_or(true)
}

/// The models `provider` is known to serve: its card's, else the catalog
/// entry's own list, else none.
pub(crate) fn models(provider: &str) -> Vec<Model> {
    if let Some(card) = card(provider)
        && !card.models.is_empty()
    {
        return card.models.clone();
    }
    crate::catalog::find(provider)
        .map(|entry| entry.models.iter().cloned().map(Model::bare).collect())
        .unwrap_or_default()
}

/// The context window the provider documents for `model`, when a card
/// names one — the probe's answer when the endpoint reports none.
pub(crate) fn context(provider: &str, model: &str) -> Option<u64> {
    let card = card(provider)?;
    card.models
        .iter()
        .chain(card.coding_plan.iter().flat_map(|plan| plan.models.iter()))
        .find(|known| known.id.eq_ignore_ascii_case(model))
        .and_then(|known| known.context)
}

/// `listed` with each id's curated label, context and price filled in where
/// the list itself named none.
pub(crate) fn annotate(provider: &str, listed: Vec<Model>) -> Vec<Model> {
    let known = models(provider);
    listed
        .into_iter()
        .map(|mut model| {
            if let Some(hint) = known.iter().find(|k| k.id == model.id) {
                model.label = model.label.or_else(|| hint.label.clone());
                model.context = model.context.or(hint.context);
                model.price = model.price.or_else(|| hint.price.clone());
            }
            model
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card file parses, every API card names a catalog provider (or
    /// `custom`), every listing is one of the three shapes, and ids are
    /// unique — the server's half of the check `app/tests` makes.
    #[test]
    fn the_card_file_is_well_formed() {
        let file: File = serde_json::from_str(CARDS_JSON).expect("model-cards.json parses");
        assert!(file.cards.len() >= 11);
        for card in &file.cards {
            match card.kind.as_str() {
                "runner" => assert!(card.provider.is_none()),
                "api" => {
                    let provider = card
                        .provider
                        .as_deref()
                        .expect("an api card names a provider");
                    assert!(
                        crate::catalog::find(provider).is_some(),
                        "{provider} is not in the catalog"
                    );
                    assert!(!card.models.is_empty(), "{provider} has no curated models");
                }
                "custom" => assert_eq!(card.provider.as_deref(), Some("custom")),
                other => panic!("unknown card kind {other}"),
            }
            if let Some(listing) = card.listing.as_deref() {
                assert!(
                    matches!(listing, "native" | "none") || listing.starts_with("https://"),
                    "{listing}"
                );
            }
        }
    }

    #[test]
    fn curated_lists_and_contexts_resolve_by_provider() {
        assert_eq!(listing("zhipu-glm-en"), Listing::None);
        assert_eq!(listing("anthropic"), Listing::Native);
        assert_eq!(
            listing("deepseek"),
            Listing::Url("https://api.deepseek.com/models".to_owned())
        );
        // A catalog provider with no card is asked natively.
        assert_eq!(listing("xai-grok"), Listing::Native);
        assert!(
            models("zhipu-glm-en")
                .iter()
                .any(|m| m.id == "glm-5.3-flash")
        );
        assert_eq!(context("zhipu-glm-en", "GLM-5.3-Flash"), Some(1_000_000));
        assert_eq!(context("zhipu-glm-en", "no-such-model"), None);
        // No card: the catalog's own list, as bare ids.
        assert!(
            models("minimax")
                .iter()
                .any(|m| m.id == "MiniMax-M2" && m.context.is_none())
        );
        assert!(models("no-such-provider").is_empty());
    }

    #[test]
    fn every_provider_streams_unless_its_card_says_not() {
        for provider in [
            "anthropic",
            "openai",
            "zhipu-glm-en",
            "custom",
            "xai-grok",
            "nope",
        ] {
            assert!(streams(provider), "{provider}");
        }
        let card: Card = serde_json::from_str(
            r#"{"kind": "api", "provider": "x", "stream": false, "models": []}"#,
        )
        .expect("card");
        assert_eq!(card.stream, Some(false));
    }

    #[test]
    fn a_listed_model_gains_the_curated_hints() {
        let listed = vec![
            Model::bare("claude-opus-5-5".to_owned()),
            Model::bare("something-new".to_owned()),
        ];
        let annotated = annotate("anthropic", listed);
        assert_eq!(annotated[0].context, Some(1_000_000));
        assert_eq!(annotated[0].price.as_deref(), Some("$4 / $20"));
        assert_eq!(annotated[1], Model::bare("something-new".to_owned()));
    }
}
