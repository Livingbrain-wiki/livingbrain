//! Recall: four strategies, fused, cut to a token budget.
//!
//! Each strategy ranks the same corpus differently, and no one of them is
//! right on its own: BM25 finds the words but not the paraphrase, cosine finds
//! the paraphrase but not the rare term, the graph finds the connection the
//! question never named, and the calendar finds the month the words skip.
//! Reciprocal rank fusion combines the four without needing their scores to be
//! commensurable — it only reads the *order* each strategy produced, so a
//! strategy can be added or swapped without retuning anything else.

use std::collections::{BTreeMap, BTreeSet};

use livingbrain_access::ScopeSet;

use crate::MemoryStore;
use crate::fact::Fact;
use crate::temporal::{self, Window};
use crate::text::{estimate_tokens, tokenize};

/// The reciprocal-rank-fusion constant. Sixty is the value from the original
/// RRF paper: large enough that the head of one ranking cannot drown out an
/// agreement further down another, small enough to stay sensitive near the
/// top.
pub const RRF_K: f32 = 60.0;

/// How many equal slices a temporal window is cut into before its results are
/// interleaved.
pub const SPREAD_BUCKETS: usize = 4;

/// Which of the four strategies put an item in the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Strategy {
    /// BM25 over the quote, subject and object.
    Keyword,
    /// Cosine similarity over embeddings.
    Semantic,
    /// One hop over shared entities.
    Graph,
    /// Inside a parsed time window, spread across it.
    Temporal,
}

/// What to recall, and what may be spent on it.
#[derive(Debug, Clone)]
pub struct RecallQuery {
    /// The question, as typed. Its tokens drive keyword and graph recall, and
    /// [`parse_window`](temporal::parse_window) reads a time out of it.
    pub text: String,
    /// An optional embedding of the question, for semantic recall. Without one
    /// — or without a fact embedding of the same length — the semantic
    /// strategy contributes nothing and the fusion is over three rankings.
    pub embedding: Option<Vec<f32>>,
    /// The most tokens the rendered result may cost. Always respected: see
    /// [`estimate_tokens`](crate::estimate_tokens).
    pub budget_tokens: usize,
    /// "Now", in unix seconds, for resolving relative time words.
    pub now: i64,
}

impl RecallQuery {
    /// A query with a budget and a clock, and no embedding.
    #[must_use]
    pub fn new(text: &str, budget_tokens: usize, now: i64) -> Self {
        Self {
            text: text.to_owned(),
            embedding: None,
            budget_tokens,
            now,
        }
    }

    /// The same query, with an embedding of the question.
    #[must_use]
    pub fn embedding(mut self, embedding: Vec<f32>) -> Self {
        self.embedding = Some(embedding);
        self
    }
}

/// One recalled fact, with the score that earned it its place.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    /// The id of the fact this item is.
    pub fact_id: String,
    /// The fused score, or the semantic score when only that strategy ran.
    pub score: f32,
    /// When the fact was said, in unix seconds.
    pub at: i64,
    /// The fact's quote, verbatim and redacted.
    pub quote: String,
    /// Where the quote can be checked.
    pub source: String,
    /// Which strategies found it.
    pub strategies: Vec<Strategy>,
}

impl Recalled {
    /// The item as it is handed to a model: date, source, quote — one line
    /// per fact, so a reader can see which thread to open.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "{} [{}] {}",
            temporal::iso_date(self.at),
            self.source,
            self.quote
        )
    }

    /// What this item costs against a [`RecallQuery::budget_tokens`].
    #[must_use]
    pub fn tokens(&self) -> usize {
        estimate_tokens(&self.render())
    }
}

/// A recall result: the items, what they cost, and the window they were cut to.
#[derive(Debug, Clone, PartialEq)]
pub struct Recall {
    /// The items, in the order they should be read.
    pub items: Vec<Recalled>,
    /// What the rendered items cost, always `<=` the query's budget.
    pub tokens_used: usize,
    /// The time window the question named, if it named one.
    pub window: Option<Window>,
}

/// The hook a reranker plugs into: Workers AI, a customer cross-encoder, a
/// learned head. Identity is the default, so an un-reranked recall is exactly
/// the fused ranking and nothing has to be special-cased.
pub trait Reranker {
    /// Reorder (or drop) the fused items. Implementations must keep the
    /// budget invariant that [`recall_with`](MemoryStore::recall_with) holds
    /// afterwards, or the caller re-cuts the list.
    fn rerank(&self, query: &RecallQuery, items: Vec<Recalled>) -> Vec<Recalled>;
}

/// The reranker that changes nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityRerank;

impl Reranker for IdentityRerank {
    fn rerank(&self, _query: &RecallQuery, items: Vec<Recalled>) -> Vec<Recalled> {
        items
    }
}

/// Fuse ranked id lists by reciprocal rank.
///
/// Each ranking contributes `1 / (k + rank)` to every id in it; ties in the
/// total are broken by id, so the same inputs always produce the same output
/// and a test can assert on an exact order.
#[must_use]
pub fn rrf_fuse(rankings: &[Vec<String>], k: f32) -> Vec<(String, f32)> {
    let mut scores: BTreeMap<&str, f32> = BTreeMap::new();
    for ranking in rankings {
        for (index, id) in ranking.iter().enumerate() {
            *scores.entry(id.as_str()).or_default() += 1.0 / (k + index as f32 + 1.0);
        }
    }
    let mut fused: Vec<(String, f32)> = scores
        .into_iter()
        .map(|(id, score)| (id.to_owned(), score))
        .collect();
    fused.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    fused
}

/// BM25 parameters: term saturation and how much a long fact is penalised.
const K1: f32 = 1.2;
const B: f32 = 0.75;

/// A BM25 index over a corpus of token lists.
struct Bm25 {
    docs: Vec<Vec<String>>,
    lengths: Vec<usize>,
    document_frequency: BTreeMap<String, usize>,
    average_length: f32,
}

impl Bm25 {
    fn build(docs: Vec<Vec<String>>) -> Self {
        let mut document_frequency = BTreeMap::new();
        for tokens in &docs {
            for token in tokens.iter().collect::<BTreeSet<_>>() {
                *document_frequency.entry(token.clone()).or_default() += 1;
            }
        }
        let total: usize = docs.iter().map(Vec::len).sum();
        Self {
            lengths: docs.iter().map(Vec::len).collect(),
            average_length: if docs.is_empty() {
                0.0
            } else {
                total as f32 / docs.len() as f32
            },
            docs,
            document_frequency,
        }
    }

    /// BM25 of one query against one document. Zero means "no shared term".
    fn score(&self, query_tokens: &[String], index: usize) -> f32 {
        let count = self.docs.len() as f32;
        if count == 0.0 || self.average_length == 0.0 {
            return 0.0;
        }
        let mut score = 0.0;
        for token in query_tokens {
            let frequency = self.docs[index]
                .iter()
                .filter(|word| *word == token)
                .count();
            if frequency == 0 {
                continue;
            }
            let document_frequency =
                self.document_frequency.get(token).copied().unwrap_or(0) as f32;
            let idf = (1.0 + (count - document_frequency + 0.5) / (document_frequency + 0.5)).ln();
            let length = self.lengths[index] as f32;
            let norm = 1.0 - B + B * length / self.average_length;
            score += idf * (frequency as f32 * (K1 + 1.0)) / (frequency as f32 + K1 * norm);
        }
        score
    }
}

/// Cosine similarity, or `None` when either vector is not usable: empty, or a
/// different length from the other (which is a different embedding model, not
/// a smaller one).
fn cosine(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.is_empty() || left.len() != right.len() {
        return None;
    }
    let (dot, left_norm, right_norm) = left
        .iter()
        .zip(right)
        .fold((0.0, 0.0, 0.0), |(dot, ln, rn), (a, b)| {
            (dot + a * b, ln + a * a, rn + b * b)
        });
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return None;
    }
    Some(dot / (left_norm.sqrt() * right_norm.sqrt()))
}

impl MemoryStore {
    /// Recall under the asker's access, with no reranker.
    #[must_use]
    pub fn recall(&self, scopes: &ScopeSet, query: &RecallQuery) -> Recall {
        self.recall_with(scopes, query, &IdentityRerank)
    }

    /// Recall under the asker's access, handing the fused list to `reranker`.
    ///
    /// Four strategies rank the same corpus — keyword (BM25), semantic
    /// (cosine), graph (seeds naming an entity the question names, then one
    /// hop out) and temporal (a parsed window, spread across it) — and are
    /// fused by reciprocal rank. The scope filter goes *inside* every
    /// strategy, on the corpus before anything is scored: a post-filter would
    /// let an out-of-scope fact push an in-scope one down the fused list,
    /// spending the budget on what is about to be removed.
    ///
    /// When the question names a time, facts outside the window are dropped
    /// and the spread decides the order rather than the fused score: coverage
    /// of the month is the answer, and a relevance ranking clusters it at the
    /// crowded recent end. The scores are still computed and reported — a
    /// reranker sees them, a caller can sort by them.
    #[must_use]
    pub fn recall_with<R: Reranker + ?Sized>(
        &self,
        scopes: &ScopeSet,
        query: &RecallQuery,
        reranker: &R,
    ) -> Recall {
        self.recall_filtered(|fact| scopes.contains(&fact.scope), query, reranker)
    }

    /// Recall over whatever `in_scope` lets this caller see, under a reranker.
    /// The scope test lives here rather than in the callers so that every
    /// read filters the corpus the same way.
    pub(crate) fn recall_filtered<R: Reranker + ?Sized>(
        &self,
        in_scope: impl Fn(&Fact) -> bool,
        query: &RecallQuery,
        reranker: &R,
    ) -> Recall {
        let corpus: Vec<&Fact> = self.facts.iter().filter(|fact| in_scope(fact)).collect();
        let index: BTreeMap<&str, usize> = corpus
            .iter()
            .enumerate()
            .map(|(position, fact)| (fact.id.as_str(), position))
            .collect();

        let window = temporal::parse_window(&query.text, query.now);
        let bm25 = Bm25::build(
            corpus
                .iter()
                .map(|fact| tokenize(&fact.searchable()))
                .collect(),
        );
        let query_tokens = tokenize(&query.text);

        let keyword = bm25_rank(&corpus, &query_tokens);
        let semantic = rank_by(&corpus, |position| {
            query.embedding.as_deref().and_then(|wanted| {
                corpus[position]
                    .embedding
                    .as_deref()
                    .and_then(|have| cosine(wanted, have))
            })
        });
        let graph = graph_rank(&corpus, &query_tokens, &bm25);
        let temporal = window
            .map(|window| spread(&corpus, window))
            .unwrap_or_default();

        let rankings = [keyword, semantic, graph, temporal]
            .iter()
            .map(|ranking| ids(&corpus, ranking))
            .collect::<Vec<_>>();
        let fused = rrf_fuse(&rankings, RRF_K);

        // With a window, only what the temporal strategy saw is eligible: the
        // window is a filter on the answer, not a hint about the ranking.
        let mut items: Vec<(usize, Recalled)> = Vec::new();
        for (id, score) in fused {
            let Some(&position) = index.get(id.as_str()) else {
                continue;
            };
            if let Some(window) = window
                && !window.contains(corpus[position].at)
            {
                continue;
            }
            let strategies = strategies_for(&rankings, &id);
            items.push((
                position,
                Recalled {
                    fact_id: id.clone(),
                    score,
                    at: corpus[position].at,
                    quote: corpus[position].quote.clone(),
                    source: corpus[position].source.clone(),
                    strategies,
                },
            ));
        }

        // Temporal spread dominates when a window was parsed.
        if let Some(window) = window {
            let spread_position: BTreeMap<usize, usize> = spread(&corpus, window)
                .into_iter()
                .enumerate()
                .map(|(slot, position)| (position, slot))
                .collect();
            items.sort_by_key(|(position, _)| spread_position.get(position).copied());
        }

        let reranked = reranker.rerank(query, items.into_iter().map(|(_, item)| item).collect());
        let (items, tokens_used) = cut_to_budget(reranked, query.budget_tokens);
        Recall {
            items,
            tokens_used,
            window,
        }
    }
}

/// Which of the four rankings placed `id`, in strategy order.
fn strategies_for(rankings: &[Vec<String>], id: &str) -> Vec<Strategy> {
    [
        Strategy::Keyword,
        Strategy::Semantic,
        Strategy::Graph,
        Strategy::Temporal,
    ]
    .iter()
    .enumerate()
    .filter(|(position, _)| rankings[*position].iter().any(|found| found == id))
    .map(|(_, strategy)| *strategy)
    .collect()
}

/// The corpus positions a keyword recall for `query_tokens` returns, best
/// first, with the facts that score nothing left out.
///
/// Crate-internal: the mental-model staleness check needs the same relevance
/// judgement the keyword strategy makes, and a second implementation of it
/// would be a second answer to "what counts as evidence for this question".
pub(crate) fn bm25_rank(corpus: &[&Fact], query_tokens: &[String]) -> Vec<usize> {
    let index = Bm25::build(
        corpus
            .iter()
            .map(|fact| tokenize(&fact.searchable()))
            .collect(),
    );
    let mut scored: Vec<(usize, f32)> = (0..corpus.len())
        .map(|position| (position, index.score(query_tokens, position)))
        .filter(|(_, score)| *score > 0.0)
        .collect();
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| corpus[left.0].id.cmp(&corpus[right.0].id))
    });
    scored.into_iter().map(|(position, _)| position).collect()
}

/// The ids of corpus positions, in order.
fn ids(corpus: &[&Fact], positions: &[usize]) -> Vec<String> {
    positions
        .iter()
        .map(|position| corpus[*position].id.clone())
        .collect()
}

/// The corpus positions `score` ranks, best first, with the unscored left out.
fn rank_by<F>(corpus: &[&Fact], mut score: F) -> Vec<usize>
where
    F: FnMut(usize) -> Option<f32>,
{
    let mut scored: Vec<(usize, f32)> = corpus
        .iter()
        .enumerate()
        .filter_map(|(position, _)| score(position).map(|value| (position, value)))
        .filter(|(_, value)| *value > 0.0)
        .collect();
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| corpus[left.0].id.cmp(&corpus[right.0].id))
    });
    scored.into_iter().map(|(position, _)| position).collect()
}

/// Seeds — facts naming an entity the question names — then one hop out.
fn graph_rank(corpus: &[&Fact], query_tokens: &[String], bm25: &Bm25) -> Vec<usize> {
    let asked: BTreeSet<&str> = query_tokens.iter().map(String::as_str).collect();
    let entities: Vec<BTreeSet<String>> = corpus
        .iter()
        .map(|fact| {
            tokenize(&fact.subject)
                .into_iter()
                .chain(tokenize(&fact.object))
                .collect()
        })
        .collect();

    let seeds: Vec<usize> = (0..corpus.len())
        .filter(|position| {
            entities[*position]
                .iter()
                .any(|entity| asked.contains(entity.as_str()))
        })
        .collect();
    let seed_set: BTreeSet<usize> = seeds.iter().copied().collect();
    let hops: Vec<usize> = (0..corpus.len())
        .filter(|position| !seed_set.contains(position))
        .filter(|position| {
            entities[*position]
                .iter()
                .any(|entity| seeds.iter().any(|seed| entities[*seed].contains(entity)))
        })
        .collect();

    let by_score = |positions: Vec<usize>| -> Vec<usize> {
        let mut ranked: Vec<(usize, f32)> = positions
            .into_iter()
            .map(|position| (position, bm25.score(query_tokens, position)))
            .collect();
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| corpus[left.0].id.cmp(&corpus[right.0].id))
        });
        ranked.into_iter().map(|(position, _)| position).collect()
    };
    let mut positions = by_score(seeds);
    positions.extend(by_score(hops));
    positions
}

/// Order the corpus so the window is *covered*: items fall into one of
/// [`SPREAD_BUCKETS`] equal time slices, and the result round-robins the
/// slices, newest first inside each — so a budget of four answers "what
/// happened in June" with one thing from the first week, one from the middle
/// and two from the last, rather than four things from the thirtieth.
fn spread(corpus: &[&Fact], window: Window) -> Vec<usize> {
    let mut buckets: Vec<Vec<usize>> = vec![Vec::new(); SPREAD_BUCKETS];
    for (position, fact) in corpus.iter().enumerate() {
        if !window.contains(fact.at) {
            continue;
        }
        let offset = fact.at - window.start;
        let bucket = (offset * SPREAD_BUCKETS as i64 / window.len_secs()) as usize;
        buckets[bucket.min(SPREAD_BUCKETS - 1)].push(position);
    }
    for bucket in &mut buckets {
        bucket.sort_by(|left, right| {
            corpus[*right]
                .at
                .cmp(&corpus[*left].at)
                .then_with(|| corpus[*left].id.cmp(&corpus[*right].id))
        });
    }
    let longest = buckets.iter().map(Vec::len).max().unwrap_or(0);
    let mut order = Vec::new();
    for round in 0..longest {
        for bucket in &buckets {
            if let Some(position) = bucket.get(round) {
                order.push(*position);
            }
        }
    }
    order
}

/// Take items in order until the budget is spent. An item that does not fit
/// is *skipped*, not a stop: a single verbose quote must not empty the tail of
/// the answer. `tokens_used <= budget_tokens` therefore holds for every
/// budget, including zero.
fn cut_to_budget(items: Vec<Recalled>, budget_tokens: usize) -> (Vec<Recalled>, usize) {
    let mut kept = Vec::new();
    let mut tokens_used = 0;
    for item in items {
        let cost = item.tokens();
        if tokens_used + cost > budget_tokens {
            continue;
        }
        tokens_used += cost;
        kept.push(item);
    }
    (kept, tokens_used)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rrf_agreement_beats_a_single_first_place() {
        let fused = rrf_fuse(
            &[vec!["a".into(), "b".into()], vec!["b".into(), "c".into()]],
            RRF_K,
        );
        let order: Vec<&str> = fused.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            order,
            ["b", "a", "c"],
            "second place twice (1/62 + 1/61) beats first place once (1/61)"
        );
        let score = |id: &str| fused.iter().find(|(found, _)| found == id).map(|(_, s)| *s);
        let expected = 1.0 / (RRF_K + 2.0) + 1.0 / (RRF_K + 1.0);
        assert!((score("b").expect("b scored") - expected).abs() < 1e-6);
    }

    #[test]
    fn rrf_breaks_a_tie_by_id() {
        let fused = rrf_fuse(
            &[vec!["b".into(), "a".into()], vec!["a".into(), "b".into()]],
            RRF_K,
        );
        assert_eq!(
            fused[0].0, "a",
            "equal scores are ordered by id, not by luck"
        );
        assert_eq!(fused[0].1, fused[1].1);
    }

    #[test]
    fn cosine_refuses_mismatched_lengths() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), None);
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0]), Some(1.0));
    }
}
