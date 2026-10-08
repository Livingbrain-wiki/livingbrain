//! `livingbrain-memory`: the memory model — typed facts, consolidated
//! observations, mental models, and recall under a token budget.
//!
//! That design is adopted from Hindsight by Vectorize
//! (<https://github.com/vectorize-io/hindsight>, MIT; see NOTICE): the ideas
//! are taken, the code is not — nothing from Hindsight is copied here.
//!
//! Pure Rust and I/O-free: the store is in memory, the clock is a number the
//! caller sets, and nothing here opens a socket, reads a file or calls a model.
//! The two neighbours are the crates that decide whether a read is allowed
//! (`livingbrain-access`) and what may be written at all (`livingbrain-redact`).
//!
//! A [`Fact`] is one claim with the exact words it was claimed in; an
//! [`Observation`] is a belief, the evidence under it and the beliefs it
//! replaced ([`consolidate`](MemoryStore::consolidate) folds facts into them);
//! a [`MentalModel`] is a question answered once, with the watermark of the
//! evidence that answer has seen; a [`Recalled`] is what one read returned.
//!
//! Every write goes through [`retain`](MemoryStore::retain), which redacts it.
//! Every read — [`recall`](MemoryStore::recall), [`reflect`](MemoryStore::reflect)
//! and every accessor that hands back stored content — takes the asker's
//! [`ScopeSet`], filtered inside each strategy on the corpus rather than on the
//! answer. A `Fact`'s `at` is unix seconds; reading a time out of a question is
//! [`temporal::parse_window`]'s job alone.

#![forbid(unsafe_code)]

mod fact;
mod mental;
mod observe;
mod recall;
pub mod temporal;
pub mod text;

use std::collections::{BTreeMap, BTreeSet};

use livingbrain_access::{Scope, ScopeSet};
use livingbrain_redact::RedactError;

pub use crate::fact::{Fact, FactKind};
pub use crate::mental::{
    MentalModel, ModelAnswer, QUESTION_OVERLAP, REFRESH_BUDGET_TOKENS, StoredAnswer,
    questions_match,
};
pub use crate::observe::{Evidence, Observation, ObservationKey, Revision};
pub use crate::recall::{
    IdentityRerank, RRF_K, Recall, RecallQuery, Recalled, Reranker, SPREAD_BUCKETS, Strategy,
    rrf_fuse,
};
pub use crate::temporal::Window;
pub use crate::text::{estimate_tokens, normalize_key, tokenize};

/// What a [`MemoryStore`] could not do. Every variant means the store is
/// exactly as it was before the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryError {
    /// The redactor would not hand the text back — an input over its cap.
    Redact(RedactError),
    /// This id is already on a stored fact. Ids are the key everything else
    /// cites by, so a collision is refused rather than resolved.
    DuplicateId(String),
    /// The asker's scopes do not include this scope, so the operation is not
    /// theirs to perform.
    ScopeNotReadable(Scope),
}

impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Redact(error) => write!(f, "refusing to store: {error}"),
            Self::DuplicateId(id) => write!(f, "a fact with id {id} is already stored"),
            Self::ScopeNotReadable(scope) => write!(f, "{scope} is outside the asker's scopes"),
        }
    }
}

impl std::error::Error for MemoryError {}

impl From<RedactError> for MemoryError {
    fn from(error: RedactError) -> Self {
        Self::Redact(error)
    }
}

/// Everything a brain knows, in memory.
///
/// Facts, observations and models, plus the bookkeeping that makes
/// [`consolidate`](Self::consolidate) safe to run twice.
#[derive(Debug, Default)]
pub struct MemoryStore {
    facts: Vec<Fact>,
    observations: BTreeMap<ObservationKey, Observation>,
    models: BTreeMap<String, MentalModel>,
    models_by_question: BTreeMap<(Scope, String), String>,
    /// Facts already folded into an observation.
    folded: BTreeSet<String>,
    /// The number of facts retained so far; the next fact's id and order.
    sequence: u64,
    /// "Now", in unix seconds. Zero until a caller sets it.
    now: i64,
}

impl MemoryStore {
    /// An empty brain.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the clock that stamps answers and resolves relative time words.
    /// A Worker and a test must both be able to say what "today" means.
    pub fn set_now(&mut self, now: i64) {
        self.now = now;
    }

    /// The store's clock, in unix seconds.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.now
    }

    /// Store a fact and return its id.
    ///
    /// Every text field — quote, subject, predicate, object and source — goes
    /// through [`livingbrain_redact::redact`] under
    /// [`Policy::Redact`](livingbrain_redact::Policy::Redact) first, and only
    /// the redacted text is kept: a secret that reached an ingest path is
    /// therefore not in memory, in the store, or in a recall.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Redact`] over
    /// [`MAX_INPUT_BYTES`](livingbrain_redact::MAX_INPUT_BYTES) — the caller
    /// fixes that by splitting the document — and [`MemoryError::DuplicateId`]
    /// when the fact carries an id that is already taken.
    pub fn retain(&mut self, fact: Fact) -> Result<String, MemoryError> {
        let clean = |text: &str| -> Result<String, MemoryError> {
            Ok(livingbrain_redact::redact(text, livingbrain_redact::Policy::Redact)?.0)
        };
        let mut fact = fact;
        fact.quote = clean(&fact.quote)?;
        fact.subject = clean(&fact.subject)?;
        fact.predicate = clean(&fact.predicate)?;
        fact.object = clean(&fact.object)?;
        fact.source = clean(&fact.source)?;

        // One monotonic counter serves both roles: it is the fact's place in
        // the retention order, and it is what an auto-assigned id is made of —
        // so an id can never be reused and a caller-supplied id can neither
        // collide with a later auto-id nor be shadowed by one.
        fact.seq = self.sequence;
        self.sequence = self.sequence.saturating_add(1);

        if fact.id.is_empty() {
            let mut candidate = format!("fact-{}", fact.seq);
            let mut bump = 1;
            while self.facts.iter().any(|stored| stored.id == candidate) {
                bump += 1;
                candidate = format!("fact-{}-{bump}", fact.seq);
            }
            fact.id = candidate;
        } else if self.facts.iter().any(|stored| stored.id == fact.id) {
            return Err(MemoryError::DuplicateId(fact.id));
        }
        let id = fact.id.clone();
        self.facts.push(fact);
        Ok(id)
    }

    /// A fact by id, if the asker may read its scope.
    #[must_use]
    pub fn fact(&self, id: &str, scopes: &ScopeSet) -> Option<&Fact> {
        self.facts
            .iter()
            .find(|fact| fact.id == id && scopes.contains(&fact.scope))
    }

    /// The retained facts the asker may read, in retention order.
    #[must_use]
    pub fn facts(&self, scopes: &ScopeSet) -> Vec<&Fact> {
        self.facts
            .iter()
            .filter(|fact| scopes.contains(&fact.scope))
            .collect()
    }

    /// The observations the asker may read, in key order: scope, then
    /// subject, then predicate.
    #[must_use]
    pub fn observations(&self, scopes: &ScopeSet) -> Vec<&Observation> {
        self.observations
            .values()
            .filter(|observation| scopes.contains(&observation.key.scope))
            .collect()
    }

    /// A mental model by id, if the asker may read its scope.
    #[must_use]
    pub fn model(&self, id: &str, scopes: &ScopeSet) -> Option<&MentalModel> {
        self.models
            .get(id)
            .filter(|model| scopes.contains(&model.scope))
    }

    /// The mental models the asker may read, in id order.
    #[must_use]
    pub fn models(&self, scopes: &ScopeSet) -> Vec<&MentalModel> {
        self.models
            .values()
            .filter(|model| scopes.contains(&model.scope))
            .collect()
    }
}

/// What a read settled on, best tier first: a stored answer beats a
/// consolidated belief, a belief beats the raw claims under it. Every tier
/// carries its citations, so the chain from an answer back to the exact words
/// in the exact thread is one hop.
#[derive(Debug, Clone, PartialEq)]
pub enum Reflection {
    /// A mental model in the asker's scopes answered this question.
    Model(ModelAnswer),
    /// The recall hits are covered by beliefs, with the evidence under them.
    Observations {
        /// The observations, in key order.
        observations: Vec<Observation>,
        /// The recalled fact ids, in recall order.
        citations: Vec<String>,
    },
    /// Nothing consolidated covers this: the raw facts, in recall order.
    Facts {
        /// The fact ids.
        citations: Vec<String>,
    },
}

impl MemoryStore {
    /// Answer a question from the best tier that can answer it, in the
    /// asker's scopes: a matching model with a stored answer, else the
    /// observations covering the recalled facts, else the facts.
    ///
    /// A stale answer is still returned, flagged
    /// [`stale`](ModelAnswer::stale); an answer that does not fit the query's
    /// budget falls through to the next tier, and one that is both stale and
    /// too big is not served at all. A stored answer is checked before recall
    /// runs: it costs a map lookup, and an already-answered question should
    /// not pay for a search.
    #[must_use]
    pub fn reflect(&self, scopes: &ScopeSet, query: &RecallQuery) -> Reflection {
        let model = self
            .models
            .values()
            .filter(|model| scopes.contains(&model.scope))
            .find(|model| questions_match(&model.question, &query.text) && model.answer.is_some());
        if let Some(answer) = model.and_then(|model| self.answer_model(&model.id, scopes))
            && answer.tokens_used <= query.budget_tokens
        {
            return Reflection::Model(answer);
        }

        let recalled = self.recall(scopes, query);
        let citations: Vec<String> = recalled
            .items
            .iter()
            .map(|item| item.fact_id.clone())
            .collect();
        let facts: BTreeSet<&str> = recalled
            .items
            .iter()
            .map(|item| item.fact_id.as_str())
            .collect();
        let observations: Vec<Observation> = self
            .observations(scopes)
            .into_iter()
            .filter(|observation| {
                observation
                    .evidence
                    .iter()
                    .any(|evidence| facts.contains(evidence.fact_id.as_str()))
            })
            .cloned()
            .collect();

        if observations.is_empty() {
            Reflection::Facts { citations }
        } else {
            Reflection::Observations {
                citations,
                observations,
            }
        }
    }
}
