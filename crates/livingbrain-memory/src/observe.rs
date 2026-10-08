//! [`Observation`]s: what several facts together support.
//!
//! A fact is a claim somebody made. An observation is a belief, with the
//! evidence under it — so it can be revised, and the belief it used to hold
//! goes into [`history`](Observation::history) with the evidence that
//! supported it. A system that only stores facts has no way to say "this was
//! true, and then it stopped being true".

use livingbrain_access::Scope;

use crate::MemoryStore;
use crate::fact::Fact;
use crate::text::normalize_key;

/// What an observation is about: a scope, a subject and a predicate.
///
/// The scope is part of the key, so two scopes can say opposite things about
/// the same subject without ever meeting. Merging them would be a leak, not a
/// deduplication.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObservationKey {
    /// The scope the observation lives in.
    pub scope: Scope,
    /// Lowercased, whitespace-collapsed subject.
    pub subject: String,
    /// Lowercased, whitespace-collapsed predicate.
    pub predicate: String,
}

impl ObservationKey {
    /// The key a fact folds into.
    #[must_use]
    pub fn for_fact(fact: &Fact) -> Self {
        Self {
            scope: fact.scope.clone(),
            subject: normalize_key(&fact.subject),
            predicate: normalize_key(&fact.predicate),
        }
    }
}

/// One fact's support for a belief: which fact, and the exact words it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The id of the fact this evidence came from.
    pub fact_id: String,
    /// That fact's quote, verbatim.
    pub quote: String,
    /// That fact's source reference.
    pub source: String,
}

/// A belief an observation held before the one it holds now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    /// The belief as it stood.
    pub belief: String,
    /// The evidence it stood on, kept so a revision is checkable.
    pub evidence: Vec<Evidence>,
    /// When it was superseded, in unix seconds.
    pub superseded_at: i64,
}

/// A consolidated belief about one `(scope, subject, predicate)`.
///
/// [`proof_count`](Observation::proof_count) counts distinct facts, so a
/// re-run of consolidation cannot inflate it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// What it is about.
    pub key: ObservationKey,
    /// What is currently believed — the object of the most recent fact folded
    /// in.
    pub belief: String,
    /// The facts supporting it, each with its exact quote.
    pub evidence: Vec<Evidence>,
    /// How many distinct facts support it.
    pub proof_count: usize,
    /// Beliefs this one replaced, oldest first.
    pub history: Vec<Revision>,
}

impl Observation {
    /// The lowercase form of the current belief, for the comparison
    /// [`consolidate`](MemoryStore::consolidate) makes.
    fn belief_key(&self) -> String {
        normalize_key(&self.belief)
    }
}

impl MemoryStore {
    /// Fold every fact retained so far into observations — what the nightly
    /// evolve calls.
    ///
    /// Safe to run on the next night, or twice in a row after a crash: each
    /// fact is folded once (the store remembers which), oldest first by `at`
    /// with retention order as the tie-break. Agreement adds evidence;
    /// disagreement *revisions* — the old belief and its evidence move to
    /// [`history`](Observation::history), so a belief is never quietly
    /// replaced. A later fact saying what a superseded revision said revives
    /// it, carrying both its old evidence and the new fact.
    pub fn consolidate(&mut self) {
        let mut pending: Vec<Pending> = self
            .facts
            .iter()
            .enumerate()
            .filter(|(_, fact)| !self.folded.contains(&fact.id))
            .map(|(_, fact)| Pending {
                key: ObservationKey::for_fact(fact),
                fact_id: fact.id.clone(),
                object: fact.object.clone(),
                quote: fact.quote.clone(),
                source: fact.source.clone(),
                at: fact.at,
                seq: fact.seq,
            })
            .collect();
        pending.sort_by_key(|pending| (pending.at, pending.seq));

        for pending in pending {
            self.folded.insert(pending.fact_id.clone());
            let key = pending.key;
            let object = pending.object.clone();
            let at = pending.at;
            let evidence_fact = Evidence {
                fact_id: pending.fact_id,
                quote: pending.quote,
                source: pending.source,
            };
            let observation = self
                .observations
                .entry(key.clone())
                .or_insert_with(|| Observation {
                    key: key.clone(),
                    belief: object.clone(),
                    evidence: vec![evidence_fact.clone()],
                    proof_count: 1,
                    history: Vec::new(),
                });

            let seen = observation
                .evidence
                .iter()
                .any(|existing| existing.fact_id == evidence_fact.fact_id);
            if seen {
                continue;
            }
            let object_key = normalize_key(&object);
            if observation.belief_key() == object_key {
                observation.evidence.push(evidence_fact);
            } else {
                let superseded = Revision {
                    belief: observation.belief.clone(),
                    evidence: std::mem::take(&mut observation.evidence),
                    superseded_at: at,
                };
                // Claiming a superseded belief again revives it rather than
                // restarting it: the new fact corroborates what it stood on.
                let revived = observation
                    .history
                    .iter()
                    .position(|revision| normalize_key(&revision.belief) == object_key)
                    .map(|position| observation.history.remove(position));
                match revived {
                    Some(Revision {
                        belief,
                        mut evidence,
                        ..
                    }) => {
                        observation.belief = belief;
                        observation.history.push(superseded);
                        evidence.push(evidence_fact);
                        observation.evidence = evidence;
                    }
                    None => {
                        observation.belief = object;
                        observation.history.push(superseded);
                        observation.evidence.push(evidence_fact);
                    }
                }
            }
            observation.proof_count = observation.evidence.len();
        }
    }
}

/// A fact waiting to be folded, detached from the store so the fold can
/// borrow `self.observations` mutably.
struct Pending {
    key: ObservationKey,
    fact_id: String,
    object: String,
    quote: String,
    source: String,
    at: i64,
    /// Retention order, so two facts said in the same second fold in the order
    /// they arrived.
    seq: u64,
}
