//! How a search query becomes blind-index lookups.
//!
//! The index stores every distinct word of a body as written (lowercased,
//! split on non-alphanumerics — [`crate::keys::tokenize`]); nothing is
//! stemmed at write time, so nothing has to be re-indexed for this to work.
//! A query is tokenized the same way, its stopwords dropped (English
//! function words and the words a question is phrased in: "what", "were",
//! "found", "the"), and each remaining word is looked up together with its
//! light singular/plural forms — `issues` also looks for `issue`, `policy`
//! for `policies`. One query word and its forms are one [`QueryTerm`]: a
//! page that holds two of the forms counts that word once.

use crate::keys;

/// The most query words one search looks up. A pasted paragraph is still a
/// search, not a statement the database has to plan around.
pub const MAX_QUERY_TERMS: usize = 12;

/// Words that say nothing about which page is meant: English function words
/// and the verbs and fillers a question is phrased with. Sorted, so a lookup
/// is a binary search.
const STOPWORDS: &[&str] = &[
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "anything",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "could",
    "describe",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "else",
    "ever",
    "every",
    "explain",
    "few",
    "find",
    "for",
    "found",
    "from",
    "further",
    "get",
    "give",
    "got",
    "had",
    "happen",
    "happened",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "him",
    "his",
    "how",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "just",
    "know",
    "let",
    "list",
    "me",
    "more",
    "most",
    "much",
    "my",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "out",
    "over",
    "own",
    "please",
    "regarding",
    "said",
    "same",
    "say",
    "she",
    "should",
    "show",
    "so",
    "some",
    "something",
    "such",
    "tell",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "us",
    "very",
    "was",
    "we",
    "were",
    "what",
    "whatever",
    "when",
    "where",
    "whether",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "would",
    "you",
    "your",
    "yours",
];

/// One word of a query and the forms of it the index is asked for. The word
/// itself is always the first form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTerm {
    pub token: String,
    pub forms: Vec<String>,
}

impl QueryTerm {
    /// Whether an already-tokenized word is one of this term's forms.
    #[must_use]
    pub fn matches(&self, word: &str) -> bool {
        self.forms.iter().any(|form| form == word)
    }
}

/// Whether `token` (already lowercased) is a stopword.
#[must_use]
pub fn is_stopword(token: &str) -> bool {
    STOPWORDS.binary_search(&token).is_ok()
}

/// The words of `query` a search looks up, in the order they were asked,
/// stopwords dropped and at most [`MAX_QUERY_TERMS`] of them. Empty when the
/// query is only stopwords and punctuation, which matches nothing.
#[must_use]
pub fn query_terms(query: &str) -> Vec<QueryTerm> {
    keys::tokenize(query)
        .into_iter()
        .filter(|token| !is_stopword(token))
        .take(MAX_QUERY_TERMS)
        .map(|token| QueryTerm {
            forms: forms(&token),
            token,
        })
        .collect()
}

/// `token` and its light singular/plural forms. Only plain ASCII words of
/// four letters or more are folded: a number, an identifier with digits, or
/// a word in another script is looked up exactly as written.
fn forms(token: &str) -> Vec<String> {
    let mut out = vec![token.to_owned()];
    if token.len() < 4 || !token.bytes().all(|b| b.is_ascii_lowercase()) {
        return out;
    }
    let sibilant = |stem: &str| {
        ["s", "x", "z", "ch", "sh"]
            .iter()
            .any(|end| stem.ends_with(end))
    };
    if let Some(stem) = token.strip_suffix("ies") {
        out.push(format!("{stem}y"));
    } else if let Some(stem) = token.strip_suffix("es") {
        if sibilant(stem) {
            out.push(stem.to_owned());
        }
        out.push(token[..token.len() - 1].to_owned());
    } else if token.ends_with('s') && !["ss", "us", "is"].iter().any(|end| token.ends_with(end)) {
        out.push(token[..token.len() - 1].to_owned());
    } else if let Some(stem) = token.strip_suffix('y')
        && !stem.ends_with(['a', 'e', 'i', 'o', 'u'])
    {
        out.push(format!("{stem}ies"));
    } else if sibilant(token) {
        out.push(format!("{token}es"));
    } else {
        out.push(format!("{token}s"));
    }
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stopword_list_is_sorted_so_the_lookup_works() {
        assert!(STOPWORDS.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn a_question_keeps_only_the_words_that_name_something() {
        let terms = query_terms("What issues were found in the EarthOS codebase?");
        let tokens: Vec<&str> = terms.iter().map(|term| term.token.as_str()).collect();
        assert_eq!(tokens, ["issues", "earthos", "codebase"]);
        assert_eq!(terms[0].forms, ["issues", "issue"]);
        assert_eq!(terms[2].forms, ["codebase", "codebases"]);
    }

    #[test]
    fn a_stopword_only_query_has_no_terms() {
        assert!(query_terms("what were the ... of it?").is_empty());
        assert!(query_terms("").is_empty());
    }

    #[test]
    fn plurals_fold_both_ways() {
        assert_eq!(forms("policies"), ["policies", "policy"]);
        assert_eq!(forms("policy"), ["policy", "policies"]);
        assert_eq!(forms("boxes"), ["boxes", "box", "boxe"]);
        assert_eq!(forms("branch"), ["branch", "branches"]);
        assert_eq!(forms("status"), ["status", "statuses"]);
        assert_eq!(forms("pages"), ["pages", "page"]);
        assert_eq!(forms("v2"), ["v2"]);
        assert_eq!(forms("key"), ["key"]);
        assert_eq!(forms("reykjavík"), ["reykjavík"]);
    }
}
