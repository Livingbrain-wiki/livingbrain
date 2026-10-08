//! The two text operations the whole model shares: a matching key and a
//! tokenizer. Both are deliberately conservative — a fact keeps the language
//! and the spelling it was written in, and only these two derivations look at
//! the text.

// The matching key for a subject, a predicate or an object: lowercased, with
//! every run of whitespace collapsed to one space and the ends trimmed.
//!
//! This is the *only* normalisation the model does. It exists so that
//! `"Ada  Lovelace"` and `"ada lovelace"` are one subject, and it is applied
//! to the key alone — the stored text is never rewritten, so the quote stays
//! exactly what the thread said.
#[must_use]
pub fn normalize_key(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            for lowered in ch.to_lowercase() {
                out.push(lowered);
            }
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Split `text` into lowercase tokens on non-alphanumeric characters.
///
/// Unicode-aware on purpose: `char::is_alphanumeric` keeps CJK, Cyrillic and
/// accented letters whole, so `東京` is one token and is matched by the same
/// code path as `june`. Nothing is stripped — a token that is not Latin is
/// still a token, because a query in one script has to find the text in it.
#[must_use]
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            for lowered in ch.to_lowercase() {
                current.push(lowered);
            }
        } else if !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// An estimate of how many tokens `text` costs a model: `ceil(chars / 4)`.
///
/// Four characters per token is the middle of the range English prose
/// actually sits in, and it is an estimate on purpose: the budget has to be
/// checkable without a tokenizer dependency, and a budget that is respected by
/// a crude estimate is respected by a real one too. It over-counts on CJK,
/// where a character is closer to a token — the safe direction for a budget.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_collapses_case_and_space() {
        assert_eq!(normalize_key("  Ada   Lovelace "), "ada lovelace");
    }

    #[test]
    fn tokenizer_keeps_non_latin_scripts() {
        assert_eq!(
            tokenize("de reboot in 東京!"),
            ["de", "reboot", "in", "東京"]
        );
    }

    #[test]
    fn token_estimate_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
