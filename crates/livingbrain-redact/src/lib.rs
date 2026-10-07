//! `livingbrain-redact`: take the secrets and the PII out of text, before it
//! is stored anywhere.
//!
//! Every ingest path in Living Brain — a Slack message, a mail, a fetched
//! page, a pasted note — runs through [`redact`] on the way in. The same
//! library runs in the Cloudflare Worker and in a CLI, so the thing that
//! decides what is stored is one piece of code, not two that can disagree.
//!
//! # The contract
//!
//! [`redact`] returns the clean text and one [`Finding`] per thing it removed.
//! A finding carries a [`Class`] and a byte span into the *original* input and
//! nothing else: no matched text, no prefix, no length, no hash. A finding is
//! therefore safe to log, to put in an error, to show in a UI. That is the
//! whole reason it is a struct of two fields and not a struct with a `String`
//! in it — the copy of the secret that would have made debugging easy is the
//! copy that ends up in a support ticket.
//!
//! # Bounded, on purpose
//!
//! Every pattern is a `regex` pattern, and `regex` is a finite automaton: a
//! match costs time linear in the input, there is no backtracking, and no
//! document can make a detector re-scan a span. Repetition is bounded
//! everywhere it could otherwise be unbounded, so a megabyte of `-----BEGIN
//! RSA PRIVATE KEY-----` is O(n) rather than O(n²).
//!
//! [`MAX_INPUT_BYTES`] caps the input, because a linear scan of a
//! 40-megabyte paste is a denial of service in a Worker with a CPU budget.
//! An input over the cap is an error, not a silent truncation: a caller
//! streaming a large document splits it on a line or record boundary first.
//!
//! # What is deliberately not caught
//!
//! Four accepted misses, each a false positive traded for a false negative.
//! Pure-hex runs, because a git SHA and a Cargo.lock checksum are 40 or 64
//! hex characters and this crate runs on text full of them. Inline
//! `data:…;base64,` payloads, because they are images in log lines. Version
//! numbers, so `1.2.3.4` stays a release — but `10.1.2.3` is still redacted,
//! since telling it from a private address needs a parser. A bare `sk-` key
//! pasted onto the end of a word. And a secret with no prefix, no label and
//! under 4 bits per byte of entropy is not a thing a regex can find at all.
//!
//! Patterns adapted from [Hindsight](https://github.com/vectorize-io/hindsight)
//! (MIT, Copyright (c) 2025 Vectorize AI, Inc.) are marked where they live in
//! `patterns.rs`, and `NOTICE` credits them.

#![forbid(unsafe_code)]

use core::fmt;
use core::ops::Range;

mod patterns;

use patterns::{Take, detectors};

/// The largest input [`redact`] will look at, in bytes.
///
/// Longer input is [`RedactError::TooLarge`] rather than a partial redaction:
/// a caller that streams a document splits it — on a line or record
/// boundary, so a secret is not cut in half — and calls [`redact`] per chunk.
pub const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// What kind of thing was removed, named for logs and for the replacement
/// token. The variants are the stable vocabulary: a workspace policy or a
/// dashboard may key off these strings, so they do not change meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Class {
    /// A GitHub personal access, OAuth, user, server or refresh token.
    GitHubToken,
    /// A GitLab personal access token.
    GitLabPat,
    /// An AWS access key id: `AKIA` for a long-lived one, `ASIA` for a
    /// session credential.
    AwsAccessKey,
    /// An AWS secret access key, found next to its label.
    AwsSecretKey,
    /// An OpenAI project, admin or legacy key.
    OpenAiKey,
    /// An Anthropic API key.
    AnthropicKey,
    /// A Google API key.
    GoogleApiKey,
    /// An npm access token.
    NpmToken,
    /// A SendGrid API key.
    SendgridKey,
    /// A Slack bot, app, refresh or legacy token.
    SlackToken,
    /// The three path segments of a Slack incoming-webhook URL. The host is
    /// kept, so the log still says which workspace.
    SlackWebhook,
    /// A Stripe secret or restricted key, or a webhook signing secret.
    StripeKey,
    /// A Polar organization, personal, OAuth or client token.
    PolarToken,
    /// A JSON Web Token.
    Jwt,
    /// A PEM-encoded private key, footer included; redacted to the end of
    /// the input when the footer is missing.
    PrivateKey,
    /// The value of a `.env`-style `KEY=value` line whose key names a secret.
    EnvSecret,
    /// The password inside a `postgres://`, `mysql://` or `mongodb://` URL.
    DatabaseUrl,
    /// A long opaque token with no known prefix: base64 or base64url, mixed
    /// case and digits, over 4 bits of entropy per byte. Pure-hex runs are
    /// excluded on purpose — see the crate docs.
    HighEntropy,
    /// An email address.
    Email,
    /// A phone number, in a leading-`+` or grouped national form.
    Phone,
    /// An IPv4 address, except loopback and the unspecified address.
    IpAddress,
    /// The username segment of a home directory path.
    HomePath,
}

impl Class {
    /// The stable name of this class, as it appears in a replacement token
    /// and in anything that logs a finding.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GitHubToken => "github_token",
            Self::GitLabPat => "gitlab_pat",
            Self::AwsAccessKey => "aws_access_key",
            Self::AwsSecretKey => "aws_secret_key",
            Self::OpenAiKey => "openai_key",
            Self::AnthropicKey => "anthropic_key",
            Self::GoogleApiKey => "google_api_key",
            Self::NpmToken => "npm_token",
            Self::SendgridKey => "sendgrid_key",
            Self::SlackToken => "slack_token",
            Self::SlackWebhook => "slack_webhook",
            Self::StripeKey => "stripe_key",
            Self::PolarToken => "polar_token",
            Self::Jwt => "jwt",
            Self::PrivateKey => "private_key",
            Self::EnvSecret => "env_secret",
            Self::DatabaseUrl => "database_url",
            Self::HighEntropy => "high_entropy",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::IpAddress => "ip_address",
            Self::HomePath => "home_path",
        }
    }

    /// Specificity, lowest first. Only ever used to break a tie between two
    /// findings over the same bytes, so the order encodes one question: does
    /// this detector know *what* the thing is, or is it guessing?
    const fn rank(self) -> u8 {
        match self {
            // `sk-ant-` is a strict subset of `sk-`, so it has to win.
            Self::AnthropicKey => 0,
            // A known provider's token, or a key with a known prefix.
            Self::GitHubToken
            | Self::GitLabPat
            | Self::AwsAccessKey
            | Self::AwsSecretKey
            | Self::OpenAiKey
            | Self::GoogleApiKey
            | Self::NpmToken
            | Self::SendgridKey
            | Self::SlackToken
            | Self::SlackWebhook
            | Self::StripeKey
            | Self::PolarToken
            | Self::Jwt
            | Self::PrivateKey => 1,
            // A shape, not an identity: the key names itself a secret.
            Self::EnvSecret | Self::DatabaseUrl => 2,
            // Structured PII beats an entropy guess about the same bytes.
            Self::Email | Self::Phone | Self::IpAddress | Self::HomePath => 3,
            // The last resort, and the only detector that fires on shape
            // alone.
            Self::HighEntropy => 4,
        }
    }
}

/// One thing that was removed, and where it was.
///
/// The span indexes the input `redact` was given, so a caller can slice the
/// original to get the redacted bytes. It is never a slice *of* the secret:
/// this struct has no field that can hold one, which is what makes a
/// `Vec<Finding>` safe to log, serialise and attach to an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What was found.
    pub class: Class,
    /// Byte range in the original input, always on `char` boundaries.
    pub span: Range<usize>,
}

/// What a workspace wants done with a finding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Policy {
    /// Remove the finding and store the rest. The default, because losing a
    /// message is worse than losing a key that can be rotated.
    #[default]
    Redact,
    /// Refuse the input outright. For workspaces that would rather reject
    /// mail from a gateway than store a redacted trace of it.
    Block,
}

/// Why [`redact`] returned no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedactError {
    /// The policy is [`Policy::Block`] and there was at least one finding.
    /// The findings are the same ones a [`Policy::Redact`] call would have
    /// returned, so a blocked message can be counted and classified without
    /// being read.
    Blocked(Vec<Finding>),
    /// The input is longer than [`MAX_INPUT_BYTES`]. Split it and call again.
    TooLarge {
        /// The input's length, in bytes.
        len: usize,
        /// The cap it exceeded.
        cap: usize,
    },
}

impl fmt::Display for RedactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Names and offsets only. The bytes they point at are the secret.
            Self::Blocked(findings) => {
                write!(f, "blocked: {} sensitive span(s) found: ", findings.len())?;
                for (index, finding) in findings.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(
                        f,
                        "{} at bytes {}..{}",
                        finding.class.as_str(),
                        finding.span.start,
                        finding.span.end
                    )?;
                }
                Ok(())
            }
            Self::TooLarge { len, cap } => write!(
                f,
                "input of {len} bytes is over the {cap}-byte cap; split it and redact each part"
            ),
        }
    }
}

impl std::error::Error for RedactError {}

/// Remove every finding from `text`.
///
/// Spans in the returned findings index `text`, and the findings are sorted by
/// where they start. On [`Policy::Block`] the input is refused rather than
/// cleaned, so nothing derived from a secret is written anywhere on the way
/// to finding that out.
///
/// # Errors
///
/// [`RedactError::TooLarge`] when `text` is over [`MAX_INPUT_BYTES`], and
/// [`RedactError::Blocked`] under [`Policy::Block`] with at least one finding.
pub fn redact(text: &str, policy: Policy) -> Result<(String, Vec<Finding>), RedactError> {
    if text.len() > MAX_INPUT_BYTES {
        return Err(RedactError::TooLarge {
            len: text.len(),
            cap: MAX_INPUT_BYTES,
        });
    }
    let findings = find(text);
    if policy == Policy::Block && !findings.is_empty() {
        return Err(RedactError::Blocked(findings));
    }
    let clean = apply(text, &findings);
    Ok((clean, findings))
}

/// Every finding, one at a time, from every detector, with overlaps resolved.
fn find(text: &str) -> Vec<Finding> {
    let mut hits: Vec<Finding> = Vec::new();
    for detector in detectors() {
        for captures in detector.regex.captures_iter(text) {
            let whole = captures.get(0).map_or(0..0, |matched| matched.range());
            let span = match detector.take {
                Take::All => whole,
                // A label pattern whose group did not capture found a label
                // and no secret; there is nothing to redact.
                Take::Labelled => match captures.get(1) {
                    Some(group) if !group.is_empty() => group.range(),
                    _ => continue,
                },
            };
            if !text.is_char_boundary(span.start) || !text.is_char_boundary(span.end) {
                continue;
            }
            // A zero-width match is not a finding. Labelled patterns that
            // found their label but captured nothing are already filtered
            // above; this is the backstop for a pattern that can match empty.
            if span.start >= span.end {
                continue;
            }
            // A replacement token is not a secret. Without this, redacting
            // twice finds the token again: an `.env` value is quoted, so the
            // token lands back inside the quotes and the value group matches
            // it on the second pass.
            if text[span.clone()].contains("[REDACTED:") {
                continue;
            }
            if let Some(keep) = detector.keep
                && !keep(text, span.clone())
            {
                continue;
            }
            hits.push(Finding {
                class: detector.class,
                span,
            });
        }
    }

    // Start order first, so one left-to-right sweep can resolve overlaps:
    // two findings that both start at the same byte cannot both be kept, and
    // the wider match at the same start is the one that covers the bytes.
    hits.sort_by(|a, b| {
        a.span
            .start
            .cmp(&b.span.start)
            .then(b.span.end.cmp(&a.span.end))
    });

    // Resolve the hits in clusters. A cluster is a run of hits where each one
    // starts before the previous ends; within a cluster the best hit wins and
    // every other hit contributes the bytes the winner does not cover, as
    // findings of its own class. The clusters are disjoint by construction, so
    // the emitted findings come out sorted, disjoint and non-empty.
    let mut kept: Vec<Finding> = Vec::new();
    let mut cluster: Vec<Finding> = Vec::new();
    let mut reach = 0;
    for hit in hits {
        if !cluster.is_empty() && hit.span.start >= reach {
            kept.append(&mut resolve(&cluster));
            cluster.clear();
        }
        reach = if cluster.is_empty() {
            hit.span.end
        } else {
            reach.max(hit.span.end)
        };
        cluster.push(hit);
    }
    kept.append(&mut resolve(&cluster));
    kept
}

/// Collapse one cluster of mutually overlapping hits into disjoint findings.
///
/// The cluster is cut at every span boundary into intervals that no hit
/// straddles, and each interval is given to the best hit covering it: the
/// lower class rank, and on a tie the longer span. The winner's class and
/// span therefore stand, and the bytes of a loser that the winner does not
/// cover come back as findings of the *loser's* class. Merging instead would
/// report `p4ssw0rd-AKIA…` as a single access key and lose the fact that most
/// of it is a database password, which is the part a reader of the log needs.
fn resolve(cluster: &[Finding]) -> Vec<Finding> {
    let Some((first, rest)) = cluster.split_first() else {
        return Vec::new();
    };
    if rest.is_empty() {
        return vec![first.clone()];
    }
    let mut edges: Vec<usize> = cluster
        .iter()
        .flat_map(|hit| [hit.span.start, hit.span.end])
        .collect();
    edges.sort_unstable();
    edges.dedup();

    let mut out: Vec<Finding> = Vec::new();
    for edge in edges.windows(2) {
        let (from, to) = (edge[0], edge[1]);
        let Some(best) = cluster
            .iter()
            .filter(|hit| hit.span.start <= from && hit.span.end >= to)
            .min_by_key(|hit| (hit.class.rank(), std::cmp::Reverse(hit.span.len())))
        else {
            continue;
        };
        // Neighbouring intervals that agree on a class are one finding: two
        // hits of the same class over the same bytes are one secret.
        match out.last_mut() {
            Some(last) if last.class == best.class && last.span.end == from => last.span.end = to,
            _ => out.push(Finding {
                class: best.class,
                span: from..to,
            }),
        }
    }
    out
}

/// `text` with each finding's span replaced by its token.
fn apply(text: &str, findings: &[Finding]) -> String {
    if findings.is_empty() {
        return text.to_owned();
    }
    let extra: usize = findings
        .iter()
        .map(|finding| {
            replacement(finding.class)
                .len()
                .saturating_sub(finding.span.len())
        })
        .sum();
    let mut clean = String::with_capacity(text.len() + extra);
    let mut at = 0;
    for finding in findings {
        clean.push_str(&text[at..finding.span.start]);
        clean.push_str(&replacement(finding.class));
        at = finding.span.end;
    }
    clean.push_str(&text[at..]);
    clean
}

/// The token that stands in for a class. It names the class and nothing else,
/// so the clean text is still readable and still says nothing secret.
fn replacement(class: Class) -> String {
    format!("[REDACTED:{}]", class.as_str())
}
