//! The detector table: one compiled `regex` per sensitive shape.
//!
//! Everything here is compiled once, lazily, the first time `redact` runs.
//! `regex` is a finite-automaton engine, so a match costs time linear in the
//! input and there is no backtracking: a hostile document cannot make a
//! detector walk the same span twice.
//!
//! ## Overlap
//!
//! Detectors are not mutually exclusive — a labelled `.env` value is also a
//! high-entropy token, and an Anthropic key is also an `sk-` key. Resolution
//! happens once, in `lib.rs`, by class rank and then match length. This file
//! only has to make sure each pattern is as tight as it can be, because a
//! loose pattern is a false positive the resolver cannot undo.

use core::ops::Range;
use std::sync::LazyLock;

use regex::Regex;

use crate::Class;

/// Which part of a match is the secret.
///
/// Most patterns match the secret and nothing else. The ones that need a
/// label in front of the secret (`.env` lines, `aws_secret_access_key = ...`)
/// capture it in group 1 and redact only that, so the log line stays readable.
#[derive(Clone, Copy)]
pub(crate) enum Take {
    /// Redact the whole match.
    All,
    /// Redact capture group 1, the labelled part of the match.
    Labelled,
}

/// A veto on a candidate the pattern matched but that is not worth redacting.
pub(crate) type Keep = fn(&str, Range<usize>) -> bool;

pub(crate) struct Compiled {
    pub(crate) class: Class,
    pub(crate) regex: Regex,
    pub(crate) take: Take,
    pub(crate) keep: Option<Keep>,
}

struct Spec {
    class: Class,
    pattern: &'static str,
    take: Take,
    keep: Option<Keep>,
}

pub(crate) fn detectors() -> &'static [Compiled] {
    static COMPILED: LazyLock<Vec<Compiled>> = LazyLock::new(|| {
        SPECS
            .iter()
            .map(|spec| Compiled {
                class: spec.class,
                // A pattern that does not compile is a build-time bug in this
                // file, not a runtime condition: there is no user input here.
                regex: Regex::new(spec.pattern)
                    .unwrap_or_else(|err| panic!("bad {} pattern: {err}", spec.class.as_str())),
                take: spec.take,
                keep: spec.keep,
            })
            .collect()
    });
    &COMPILED
}

const SPECS: &[Spec] = &[
    // ---------------------------------------------------------------------
    // Ported from Hindsight (MIT, Copyright (c) 2025 Vectorize AI, Inc.).
    //
    // Source: `hindsight-api-slim/hindsight_api/extensions/memory_defense.py`
    // (`_REDACTION_PATTERNS`), at
    // https://github.com/vectorize-io/hindsight/tree/3d39e6f452c6119ebbca34a6cb30a1e5f0a6edec
    //
    // Adapted, not copied verbatim: the boundary wrapper above replaces
    // Hindsight's Python lookarounds, the two `sk-` OpenAI shapes are one
    // alternation instead of an ordered pair of substitutions, the AWS
    // access-key and Slack token prefixes are merged into one class each, and
    // the Luhn and UUID guards on Hindsight's credit-card pattern have no
    // counterpart here because this crate has no card class. Hindsight
    // applies its patterns as ordered substitutions over mutated text; this
    // crate collects spans and resolves overlaps, so pattern order is not
    // load-bearing.
    // ---------------------------------------------------------------------
    // Every pattern below has a distinctive prefix and is matched with no
    // left boundary at all. A boundary would buy nothing — `ghp_` does not
    // occur in a word by accident — and would cost a real secret every time
    // one token is glued to another, as two keys on one line are.
    // ---------------------------------------------------------------------
    // Anthropic outranks the OpenAI one because `sk-ant-` is a strict subset
    // of `sk-`, and both match the same span.
    Spec {
        class: Class::AnthropicKey,
        pattern: r"sk-ant-[A-Za-z0-9_-]{20,}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::OpenAiKey,
        pattern: r"sk-proj-[A-Za-z0-9_-]{48,}|sk-admin-[A-Za-z0-9_-]{40,}",
        take: Take::All,
        keep: None,
    },
    // The bare `sk-` key is the one shape here with no distinctive prefix, so
    // it is the one that needs a boundary check. `sk-` appears inside words
    // far more often than it appears as a key, so `keep_bare_key` vetoes a
    // match that is glued to a letter. Hindsight anchors this on
    // `(?<![A-Za-z0-9_])`; `regex` has no lookaround, and consuming the
    // boundary instead would drop the second of two keys on one line.
    Spec {
        class: Class::OpenAiKey,
        pattern: r"sk-[A-Za-z0-9_-]{20,}",
        take: Take::All,
        keep: Some(keep_bare_key),
    },
    Spec {
        class: Class::GoogleApiKey,
        pattern: r"AIza[0-9A-Za-z_-]{35}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::GitHubToken,
        pattern: r"github_pat_[A-Za-z0-9_]{50,}|gh[pousr]_[A-Za-z0-9]{36}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::GitLabPat,
        pattern: r"glpat-[A-Za-z0-9_-]{20,}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::NpmToken,
        pattern: r"npm_[A-Za-z0-9]{30,}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::SendgridKey,
        pattern: r"SG\.[A-Za-z0-9_-]{22}\.[A-Za-z0-9_-]{43}",
        take: Take::All,
        keep: None,
    },
    // `AKIA` is a long-lived access key id, `ASIA` a temporary session one.
    // Same shape, same handling, so one class — and no boundary, so that
    // `AKIA…ASIA…` on one line is two findings rather than one.
    Spec {
        class: Class::AwsAccessKey,
        pattern: r"(?:AKIA|ASIA)[0-9A-Z]{16}",
        take: Take::All,
        keep: None,
    },
    // No prefix to anchor on: the label is the only evidence, so the label is
    // required. The 40 characters are what makes AWS a secret access key, and
    // the run after them is taken too — a key is not exactly 40 characters in
    // every config file, and stopping at 40 would leave the rest in clear.
    Spec {
        class: Class::AwsSecretKey,
        // Case-insensitive because AWS writes the label every way: the
        // console says `aws_secret_access_key`, Terraform says
        // `AWS_SECRET_ACCESS_KEY`, and a YAML file may say either.
        pattern: r#"(?i)aws[^\n]{0,24}?access[\s_-]?key[\s_-]?[:=][\s"']*([A-Za-z0-9/+=]{40,200})"#,
        take: Take::Labelled,
        keep: None,
    },
    Spec {
        class: Class::StripeKey,
        pattern: r"(?:sk|rk)_(?:live|test)_[A-Za-z0-9]{20,}|whsec_[A-Za-z0-9]{16,}",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::SlackToken,
        pattern: r"xox[abprs]-[0-9A-Za-z-]{10,}",
        take: Take::All,
        keep: None,
    },
    // The host is kept: it says which workspace the hook belongs to without
    // saying the hook. The three path segments are the secret.
    Spec {
        class: Class::SlackWebhook,
        pattern: r"https://hooks\.slack\.com/services/(T[A-Za-z0-9_]{8,}/B[A-Za-z0-9_]{8,}/[A-Za-z0-9_]{20,})",
        take: Take::Labelled,
        keep: None,
    },
    Spec {
        class: Class::Jwt,
        pattern: r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}",
        take: Take::All,
        keep: None,
    },
    // The body is an alternation, tried left first: stop at the footer's
    // `END`, and if there is no `END` at all, run to the end of the input.
    // A pasted key with its footer cut off must not survive because the last
    // line of the paste was truncated, and the run-to-end branch is
    // unreachable twice: a match that takes it consumes the rest of the haystack.
    Spec {
        class: Class::PrivateKey,
        pattern: concat!(
            r"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
            r"(?s:.*?-----END (?:[A-Z0-9]+ )*PRIVATE KEY(?: BLOCK)?-----",
            r"|.*)",
        ),
        take: Take::All,
        keep: None,
    },
    // ---------------------------------------------------------------------
    // This crate's own detectors. Not from Hindsight.
    // ---------------------------------------------------------------------
    // Polar (polar.sh): organization access tokens, personal access tokens,
    // the OAuth2 access/refresh pair and the client secret all carry a
    // `polar_` prefix. The key is the value, so only the value goes.
    Spec {
        class: Class::PolarToken,
        pattern: r"polar_(?:oat|pat|at_[uo]|rt_[uo]|cs)_[A-Za-z0-9]{20,}",
        take: Take::All,
        keep: None,
    },
    // `KEY=value` / `export KEY="value"`, uppercase only, at the start of a
    // line. The value alone is group 1. An empty value does not match: a
    // placeholder is not a secret and redacting it teaches the reader that
    // `[REDACTED:...]` does not mean "this was set".
    Spec {
        class: Class::EnvSecret,
        pattern: concat!(
            r"(?m)^[ \t]*(?:export[ \t]+)?",
            r"[A-Za-z_][A-Za-z0-9_]*(?:SECRET|TOKEN|PASSWORD|PASSWD|PWD|API_?KEY|PRIVATE|CREDENTIAL|AUTH)[A-Za-z0-9_]*",
            r"[ \t]*=[ \t]*",
            r#"("[^"\n]+"|'[^'\n]+'|[^\s#\n]+)"#,
        ),
        take: Take::Labelled,
        keep: None,
    },
    // `DATABASE_URL=postgres://user:hunter2@host/db` — the password is the
    // secret, and the rest of the URL is the useful part of the log line.
    Spec {
        class: Class::DatabaseUrl,
        pattern: r"(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?)://[^\s:/@]+:([^\s/@]+)@[^\s]+",
        take: Take::Labelled,
        keep: None,
    },
    // Only the username segment: `/home/[REDACTED:home_path]/src`, which still
    // says the file was under somebody's home directory.
    Spec {
        class: Class::HomePath,
        pattern: r"/(?:home|Users)/([A-Za-z0-9._-]{1,64})",
        take: Take::Labelled,
        keep: None,
    },
    Spec {
        class: Class::HomePath,
        pattern: r"[A-Za-z]:\\Users\\([A-Za-z0-9._-]{1,64})",
        take: Take::Labelled,
        keep: None,
    },
    // Every quantifier is bounded: an unbounded one is what turns a long
    // document of addresses into quadratic work.
    Spec {
        class: Class::Email,
        pattern: concat!(
            r"[A-Za-z0-9._%+-]{1,64}@",
            r"[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?",
            r"(?:\.[A-Za-z0-9](?:[A-Za-z0-9-]{0,61}[A-Za-z0-9])?){0,8}",
            r"\.[A-Za-z]{2,24}",
        ),
        take: Take::All,
        keep: None,
    },
    // A leading `+` immediately followed by a digit, or a grouped national
    // form. Deliberately narrow: bare digit runs are left alone, because
    // "1.2.3.4.5" and a 200-line diff are not phone numbers. The digit right
    // after the `+` is required, or `let x = 1 + 1234567890;` is a number.
    Spec {
        class: Class::Phone,
        pattern: r"\+[0-9](?:[0-9()\- .]?[0-9]){7,14}|\([0-9]{3}\)[ \-]?[0-9]{3}[ \-][0-9]{4}\b|\b[0-9]{3}-[0-9]{3}-[0-9]{4}\b",
        take: Take::All,
        keep: None,
    },
    Spec {
        class: Class::IpAddress,
        pattern: concat!(
            r"(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])",
            r"(?:\.(?:25[0-5]|2[0-4][0-9]|1[0-9][0-9]|[1-9][0-9]|[0-9])){3}",
        ),
        take: Take::All,
        keep: Some(keep_public_ip),
    },
    Spec {
        class: Class::HighEntropy,
        pattern: "[A-Za-z0-9+/_]{32,512}",
        take: Take::All,
        keep: Some(keep_interesting_entropy),
    },
];

/// Drop the dotted quads that are not addresses: `0.0.0.0` (a bind address)
/// and `127.0.0.0/8` (loopback), and a version number. Redacting `0.0.0.0` on
/// every local address in a log is noise, and `1.2.3.4` in a changelog is a
/// release, not a host.
///
/// The remaining miss, stated plainly: `10.1.2.3` is a plausible version and
/// is still redacted, because telling a two-digit-leading version from a
/// private address needs a parser, not a regex.
fn keep_public_ip(haystack: &str, span: Range<usize>) -> bool {
    let text = &haystack[span.clone()];
    if text == "0.0.0.0" || text.starts_with("127.") {
        return false;
    }
    // A version: every octet is a single digit, or a `v` tag sits in front.
    if text.split('.').all(|octet| octet.len() == 1) {
        return false;
    }
    if haystack[..span.start].ends_with(['v', 'V']) {
        return false;
    }
    // A longer dotted-decimal run (`1.2.3.4.5`, a syslog structured-data
    // field) is not an address. `regex` has no trailing lookahead, so the
    // next character is checked here.
    !haystack[span.end..]
        .chars()
        .next()
        .is_some_and(|c| c == '.' || c.is_ascii_digit())
}

/// The boundary check for the one shape here with no distinctive prefix.
///
/// `sk-` turns up inside ordinary words, so a match glued to a letter is
/// dropped. Two keys on one line need no special case: the token class
/// includes `-`, so the first match runs greedily through the second `sk-`
/// and covers both. The accepted miss, stated plainly: a key pasted straight
/// onto the end of a word, `xxxsk-…`, is not found.
fn keep_bare_key(haystack: &str, span: Range<usize>) -> bool {
    !haystack[..span.start]
        .chars()
        .next_back()
        .is_some_and(|previous| previous.is_ascii_alphanumeric() || previous == '_')
}

/// A long opaque token, and only when it looks like one.
///
/// Two classes of thing are skipped on purpose, and both are false-negative
/// trades a workspace should know about. A pure-hex run of 32 or more
/// characters is never flagged, because that is what a git SHA, a Cargo.lock
/// checksum and a SHA-256 digest all look like; flagging them would make
/// every ingest path in this repo unreadable, so a hex secret survives. An
/// inline `data:…;base64,` payload is never flagged, because it is an image
/// in a log line, not a credential — which means a secret smuggled inside
/// one survives too. A filesystem path is not spared in general: a run of
/// path segments that mixes cases and digits passes this test and *is*
/// flagged, and only a path with a lowercase segment, a dot, or too few
/// mixed-case characters survives.
fn keep_interesting_entropy(haystack: &str, span: Range<usize>) -> bool {
    let text = haystack.get(span.clone()).unwrap_or_default();
    if haystack[..span.start].ends_with(";base64,") || inside_a_url(haystack, span.start) {
        return false;
    }
    let mut upper = false;
    let mut lower = false;
    let mut digit = false;
    for byte in text.bytes() {
        upper |= byte.is_ascii_uppercase();
        lower |= byte.is_ascii_lowercase();
        digit |= byte.is_ascii_digit();
    }
    // A real base64 key is a mix; a long path, a hostname or an identifier is
    // usually all one case, and those are not secrets.
    upper && lower && digit && shannon_entropy(text) >= 4.0
}

/// Is this candidate part of a URL rather than a token standing on its own?
///
/// A webhook path, a route or a filename is high-entropy too, and redacting
/// `https://hooks.slack.com/services/…` as one blob throws away the host that
/// makes the line worth reading. The scan walks back over the characters a URL
/// is made of and stops at anything that is not one — a quote, a space, or the
/// `=` of a query string, so `?token=…` is still flagged. The walk is capped
/// so that a megabyte of dotted decimals cannot make it quadratic.
fn inside_a_url(haystack: &str, start: usize) -> bool {
    const SCAN: usize = 256;
    let head = haystack.as_bytes().get(..start).unwrap_or_default();
    // The window is cut first and walked second. Truncating the walk
    // afterwards would bound the loop and not the search, which turns a
    // megabyte of text into a megabyte of scanning per candidate.
    let head = &head[head.len().saturating_sub(SCAN)..];
    // The walk is the run of URL characters *immediately* in front of the
    // candidate, so it is cut from the end down to the first character that is
    // not one. Keeping the whole window instead would make every candidate
    // look like a URL as soon as the document mentioned a hostname anywhere.
    let mut from = head.len();
    for (index, &byte) in head.iter().enumerate().rev() {
        if !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'/' | b'.' | b':')) {
            break;
        }
        from = index;
    }
    let walk = &head[from..];
    walk.contains(&b'.') || walk.windows(3).any(|window| window == b"://")
}

/// Shannon entropy, in bits per byte.
fn shannon_entropy(text: &str) -> f64 {
    let mut counts = [0u32; 256];
    for &byte in text.as_bytes() {
        counts[usize::from(byte)] += 1;
    }
    let length = text.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let p = f64::from(count) / length;
            -p * p.log2()
        })
        .sum()
}
