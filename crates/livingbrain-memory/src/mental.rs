//! [`MentalModel`]s: a question the brain has already answered once.
//!
//! Recall is a search and a model is a *decision*: "where does Ada live" has
//! an answer that changes four times a year, and re-deriving it on every
//! question costs a model call and gets a different answer each time. A model
//! stores the answer, what it was written from, and the watermark of the
//! evidence it had seen — so it can be answered cheaply and refreshed only
//! when the evidence has actually moved on.

use std::collections::BTreeSet;

use livingbrain_access::{Scope, ScopeSet};

use crate::MemoryError;
use crate::MemoryStore;
use crate::fact::Fact;
use crate::recall::{IdentityRerank, RecallQuery, Recalled};
use crate::text::{estimate_tokens, normalize_key, tokenize};

/// The budget a refresh recalls under when the caller names none: a model
/// answer is a paragraph written from a handful of facts.
pub const REFRESH_BUDGET_TOKENS: usize = 512;

/// How much two questions have to overlap before a stored answer is served for
/// a new one — Jaccard over tokens. A false match is a wrong answer with
/// citations on it, so the threshold is high and equality comes first.
pub const QUESTION_OVERLAP: f64 = 0.6;

/// An answer a model wrote, with what it wrote it from. `refreshed_at` and
/// `watermark` are different instants: the answer was written now, from the
/// evidence as it stood at the watermark — which is what
/// [`stale_models`](MemoryStore::stale_models) compares newer facts against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAnswer {
    pub text: String,
    /// The fact and observation ids it was written from.
    pub citations: Vec<String>,
    /// When it was written, in unix seconds.
    pub refreshed_at: i64,
    /// The newest evidence seen for the question, in unix seconds.
    pub watermark: i64,
}

/// A question worth keeping an answer to. Evidence for it is drawn from
/// `scope` alone, so a model is never answered from somewhere the model may
/// not draw on — even if the asker asking the question may.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentalModel {
    /// Assigned by [`define_model`](MemoryStore::define_model).
    pub id: String,
    pub scope: Scope,
    pub question: String,
    pub answer: Option<StoredAnswer>,
}

/// An answer handed back to a caller, with the two numbers that matter: what
/// it cost, and whether it can still be trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelAnswer {
    pub text: String,
    pub citations: Vec<String>,
    /// An estimate of the stored answer's tokens —
    /// [`ceil(chars / 4)`](crate::estimate_tokens), the heuristic the recall
    /// budget uses. No model was called to produce this: it was read.
    pub tokens_used: usize,
    /// Whether evidence the question would recall is newer than the
    /// [`watermark`](StoredAnswer::watermark).
    pub stale: bool,
}

/// Whether a stored answer still matches its question, or its replacement.
///
/// Equality of the [normalised](normalize_key) forms, or token overlap of at
/// least [`QUESTION_OVERLAP`]. Both tests are cheap and deliberately blunt:
/// this decides whether an answer is served or the memory is searched.
#[must_use]
pub fn questions_match(question: &str, asked: &str) -> bool {
    if normalize_key(question) == normalize_key(asked) {
        return true;
    }
    let left: BTreeSet<String> = tokenize(question).into_iter().collect();
    let right: BTreeSet<String> = tokenize(asked).into_iter().collect();
    if left.is_empty() || right.is_empty() {
        return false;
    }
    let shared = left.intersection(&right).count();
    f64::from(u32::try_from(shared).unwrap_or(u32::MAX))
        / (left.len() + right.len() - shared) as f64
        >= QUESTION_OVERLAP
}

impl MemoryStore {
    /// Declare a question this brain should be able to answer, and return its
    /// id. Asking for the same question in the same scope again returns the
    /// same model rather than accumulating two that disagree.
    pub fn define_model(&mut self, scope: Scope, question: &str) -> String {
        let key = (scope.clone(), normalize_key(question));
        if let Some(id) = self.models_by_question.get(&key) {
            return id.clone();
        }
        let id = format!("model-{}", self.models.len());
        self.models_by_question.insert(key, id.clone());
        self.models.insert(
            id.clone(),
            MentalModel {
                id: id.clone(),
                scope,
                question: question.to_owned(),
                answer: None,
            },
        );
        id
    }

    /// The stored answer for a model, with its staleness computed now.
    ///
    /// `None` for a model that does not exist, one the asker may not read, or
    /// one that has never been answered — an unanswered question is not an
    /// empty answer.
    #[must_use]
    pub fn answer_model(&self, id: &str, scopes: &ScopeSet) -> Option<ModelAnswer> {
        let model = self.models.get(id).filter(|m| scopes.contains(&m.scope))?;
        let stored = model.answer.as_ref()?;
        Some(ModelAnswer {
            text: stored.text.clone(),
            citations: stored.citations.clone(),
            tokens_used: estimate_tokens(&stored.text),
            stale: self.is_stale(model),
        })
    }

    /// The models in the asker's scopes whose evidence has moved past their
    /// watermark, oldest first.
    #[must_use]
    pub fn stale_models(&self, scopes: &ScopeSet) -> Vec<String> {
        self.models
            .values()
            .filter(|model| scopes.contains(&model.scope))
            .filter(|model| self.is_stale(model))
            .map(|model| model.id.clone())
            .collect()
    }

    /// Whether any fact the model's question would recall is newer than its
    /// watermark.
    fn is_stale(&self, model: &MentalModel) -> bool {
        let Some(stored) = &model.answer else {
            return false;
        };
        self.evidence_for(model)
            .iter()
            .any(|fact| fact.at > stored.watermark)
    }

    /// The facts in the model's *own* scope that a keyword recall for its
    /// question would return — the same BM25 the keyword strategy uses, so
    /// "relevant evidence" means one thing here and another nowhere. Uncut by
    /// any budget: a fact the writer never saw is still evidence, and a
    /// watermark taken from the cut would leave the model stale forever.
    fn evidence_for(&self, model: &MentalModel) -> Vec<&Fact> {
        let corpus: Vec<&Fact> = self
            .facts
            .iter()
            .filter(|fact| fact.scope == model.scope)
            .collect();
        crate::recall::bm25_rank(&corpus, &tokenize(&model.question))
            .into_iter()
            .map(|position| corpus[position])
            .collect()
    }

    /// Re-answer a model: recall for its question, hand the recalled items to
    /// `writer` — the model call in production, a closure in a test — and store
    /// what it wrote.
    ///
    /// The citations and the watermark come from two different sets on
    /// purpose. The writer saw the budget-cut items, so those are the
    /// citations; the watermark is the newest evidence that existed for the
    /// question at all, uncut, because that is what "this answer accounts for
    /// everything up to here" has to mean for a model to ever stop being
    /// stale. Evidence is drawn from the model's own scope even where the
    /// asker may read more, so an answer written by someone who can see two
    /// channels is not later served to someone who can see one.
    ///
    /// # Errors
    ///
    /// [`MemoryError::ScopeNotReadable`] when the asker's scopes do not
    /// include the model's. `Ok(None)` for a model that does not exist.
    pub fn refresh_model<F>(
        &mut self,
        id: &str,
        scopes: &ScopeSet,
        writer: F,
    ) -> Result<Option<StoredAnswer>, MemoryError>
    where
        F: FnOnce(&[Recalled]) -> String,
    {
        let Some(model) = self.models.get(id) else {
            return Ok(None);
        };
        if !scopes.contains(&model.scope) {
            return Err(MemoryError::ScopeNotReadable(model.scope.clone()));
        }
        let model = model.clone();
        let query = RecallQuery::new(&model.question, REFRESH_BUDGET_TOKENS, self.now);
        let recalled =
            self.recall_filtered(|fact| fact.scope == model.scope, &query, &IdentityRerank);

        let watermark = self
            .evidence_for(&model)
            .iter()
            .map(|fact| fact.at)
            .max()
            .unwrap_or_default();
        let stored = StoredAnswer {
            text: writer(&recalled.items),
            citations: recalled
                .items
                .iter()
                .map(|item| item.fact_id.clone())
                .collect(),
            refreshed_at: self.now,
            watermark,
        };
        if let Some(model) = self.models.get_mut(id) {
            model.answer = Some(stored.clone());
        }
        Ok(Some(stored))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_match_on_the_normalised_form() {
        assert!(questions_match(
            "Where  does Ada live?",
            "where does ada live"
        ));
    }

    #[test]
    fn questions_match_on_overlap() {
        assert!(questions_match(
            "does Ada live in Amsterdam",
            "does Ada live in Berlin"
        ));
    }

    #[test]
    fn unrelated_questions_do_not_match() {
        assert!(!questions_match(
            "where does Ada live",
            "what happened in June"
        ));
    }
}
