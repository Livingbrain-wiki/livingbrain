//! Webhook intake, as pure functions: verify the signature over the raw body,
//! then read which delivery, event and installation it is, then read the
//! payload for that event.
//!
//! The order is the contract. Verify first, over the exact bytes GitHub sent;
//! a body that has been parsed and re-serialized no longer matches its
//! signature. Only a verified body is parsed, and the delivery id it carries
//! is the replay key a caller stores to refuse a second copy.

use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::api::{IssueComment, Item, PullReview, ReviewComment, User};
use crate::hex;

/// The header carrying `sha256=<hex>` over the raw body. The legacy `sha1`
/// header is never consulted.
pub const SIGNATURE_HEADER: &str = "X-Hub-Signature-256";
/// The header naming the event, such as `pull_request`.
pub const EVENT_HEADER: &str = "X-GitHub-Event";
/// The header holding a GUID per delivery: the replay key.
pub const DELIVERY_HEADER: &str = "X-GitHub-Delivery";

/// The longest delivery id accepted. GitHub sends a GUID (36 bytes); the
/// bound keeps a hostile header from becoming a long stored key.
const MAX_DELIVERY_ID_LEN: usize = 128;

type HmacSha256 = Hmac<Sha256>;

/// The events this connection subscribes to. Anything else is refused at
/// intake, so a new subscription on the GitHub App is a deliberate change
/// here, not a payload that quietly parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    Push,
    Issues,
    IssueComment,
    PullRequest,
    PullRequestReview,
    PullRequestReviewComment,
    Release,
    Installation,
    InstallationRepositories,
}

impl Event {
    /// The event named by an `X-GitHub-Event` value, if it is on the list.
    #[must_use]
    pub fn from_header(value: &str) -> Option<Self> {
        Some(match value {
            "push" => Self::Push,
            "issues" => Self::Issues,
            "issue_comment" => Self::IssueComment,
            "pull_request" => Self::PullRequest,
            "pull_request_review" => Self::PullRequestReview,
            "pull_request_review_comment" => Self::PullRequestReviewComment,
            "release" => Self::Release,
            "installation" => Self::Installation,
            "installation_repositories" => Self::InstallationRepositories,
            _ => return None,
        })
    }

    /// The wire name GitHub sends.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Issues => "issues",
            Self::IssueComment => "issue_comment",
            Self::PullRequest => "pull_request",
            Self::PullRequestReview => "pull_request_review",
            Self::PullRequestReviewComment => "pull_request_review_comment",
            Self::Release => "release",
            Self::Installation => "installation",
            Self::InstallationRepositories => "installation_repositories",
        }
    }
}

/// Why a signature was refused. Distinct variants, so a log line can say
/// whether GitHub sent nothing, sent another algorithm, or sent a digest that
/// is wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// No secret is configured. HMAC accepts an empty key, so without this
    /// check an unconfigured connection would verify a forged body.
    EmptySecret,
    /// The signature header is absent or blank.
    MissingSignature,
    /// The header names an algorithm other than `sha256`.
    UnsupportedAlgorithm,
    /// The header has no `=`, or its digest is not 64 hex digits.
    MalformedSignature,
    /// The digest is well-formed and does not match this body under this
    /// secret.
    Mismatch,
}

/// Verifies `X-Hub-Signature-256` over the raw `body`.
///
/// The comparison is `Mac::verify_slice`, which is constant-time; the
/// expected digest is never compared with `==`.
///
/// # Errors
///
/// See [`VerifyError`] for each refusal.
pub fn verify(secret: &[u8], body: &[u8], signature_header: &str) -> Result<(), VerifyError> {
    if secret.is_empty() {
        return Err(VerifyError::EmptySecret);
    }
    let header = signature_header.trim();
    if header.is_empty() {
        return Err(VerifyError::MissingSignature);
    }
    let (algorithm, digest) = header
        .split_once('=')
        .ok_or(VerifyError::MalformedSignature)?;
    if algorithm != "sha256" {
        return Err(VerifyError::UnsupportedAlgorithm);
    }
    let expected = hex::decode(digest)
        .filter(|bytes| bytes.len() == 32)
        .ok_or(VerifyError::MalformedSignature)?;
    let mut mac = HmacSha256::new_from_slice(secret).map_err(|_| VerifyError::EmptySecret)?;
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| VerifyError::Mismatch)
}

/// Why a delivery was refused at intake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookError {
    /// A required header is absent.
    MissingHeader(&'static str),
    /// The event is not on the subscribed list.
    UnsupportedEvent(String),
    /// The delivery id is empty, too long, or not printable ASCII.
    InvalidDeliveryId,
    /// The body is not the JSON the event needs.
    InvalidJson(String),
}

/// The envelope of a verified delivery: enough to route and dedupe it before
/// the payload is read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// The `X-GitHub-Delivery` GUID. Store it: a second delivery with the
    /// same id is a replay (GitHub retries) and must not be applied twice.
    pub id: String,
    pub event: Event,
    /// The installation the event came from, when the payload names one.
    pub installation_id: Option<u64>,
    /// `owner/name`, when the event is about a repository.
    pub repo_full_name: Option<String>,
}

#[derive(Deserialize)]
struct Envelope {
    installation: Option<InstallationRef>,
    repository: Option<RepoPayload>,
}

impl Delivery {
    /// Reads the envelope of a delivery whose signature has already been
    /// verified with [`verify`].
    ///
    /// # Errors
    ///
    /// See [`WebhookError`].
    pub fn parse(
        event: Option<&str>,
        delivery: Option<&str>,
        body: &[u8],
    ) -> Result<Self, WebhookError> {
        let event_raw = event.ok_or(WebhookError::MissingHeader(EVENT_HEADER))?;
        let id = delivery.ok_or(WebhookError::MissingHeader(DELIVERY_HEADER))?;
        if !valid_delivery_id(id) {
            return Err(WebhookError::InvalidDeliveryId);
        }
        let event = Event::from_header(event_raw)
            .ok_or_else(|| WebhookError::UnsupportedEvent(event_raw.to_owned()))?;
        let envelope: Envelope = serde_json::from_slice(body)
            .map_err(|err| WebhookError::InvalidJson(err.to_string()))?;
        Ok(Self {
            id: id.to_owned(),
            event,
            installation_id: envelope.installation.map(|installation| installation.id),
            repo_full_name: envelope.repository.map(|repo| repo.full_name),
        })
    }
}

fn valid_delivery_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_DELIVERY_ID_LEN && id.bytes().all(|b| b.is_ascii_graphic())
}

/// A repository as a payload names it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RepoPayload {
    pub full_name: String,
}

/// The installation a payload names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct InstallationRef {
    pub id: u64,
}

/// The installation object on the installation events, with its account.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationInfo {
    pub id: u64,
    pub account: Option<User>,
}

/// A push. Commits carry the id and message only; the author object, which
/// includes an email, is not modelled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PushPayload {
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub before: String,
    pub after: String,
    #[serde(default)]
    pub created: bool,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub forced: bool,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
    #[serde(default)]
    pub commits: Vec<PushCommit>,
}

/// One commit in a push.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PushCommit {
    pub id: String,
    pub message: String,
    pub timestamp: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssuesPayload {
    pub action: String,
    pub issue: Item,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueCommentPayload {
    pub action: String,
    pub issue: Item,
    pub comment: IssueComment,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestPayload {
    pub action: String,
    pub number: u64,
    pub pull_request: Item,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestReviewPayload {
    pub action: String,
    pub review: PullReview,
    pub pull_request: Item,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestReviewCommentPayload {
    pub action: String,
    pub comment: ReviewComment,
    pub pull_request: Item,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

/// The release object on a release event.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Release {
    pub id: u64,
    pub tag_name: String,
    pub name: Option<String>,
    pub body: Option<String>,
    pub html_url: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    pub published_at: Option<String>,
    pub author: Option<User>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReleasePayload {
    pub action: String,
    pub release: Release,
    pub repository: RepoPayload,
    pub installation: Option<InstallationRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationPayload {
    pub action: String,
    pub installation: InstallationInfo,
    #[serde(default)]
    pub repositories: Vec<RepoPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct InstallationRepositoriesPayload {
    pub action: String,
    pub installation: InstallationInfo,
    #[serde(default)]
    pub repositories_added: Vec<RepoPayload>,
    #[serde(default)]
    pub repositories_removed: Vec<RepoPayload>,
}

/// A payload for one of the subscribed events. The variant is the event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Payload {
    Push(Box<PushPayload>),
    Issues(Box<IssuesPayload>),
    IssueComment(Box<IssueCommentPayload>),
    PullRequest(Box<PullRequestPayload>),
    PullRequestReview(Box<PullRequestReviewPayload>),
    PullRequestReviewComment(Box<PullRequestReviewCommentPayload>),
    Release(Box<ReleasePayload>),
    Installation(Box<InstallationPayload>),
    InstallationRepositories(Box<InstallationRepositoriesPayload>),
}

/// Reads the payload for `event` from a verified body.
///
/// # Errors
///
/// [`WebhookError::InvalidJson`] when the body does not have the fields the
/// event needs.
pub fn parse_payload(event: Event, body: &[u8]) -> Result<Payload, WebhookError> {
    fn read<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<Box<T>, WebhookError> {
        serde_json::from_slice(body)
            .map(Box::new)
            .map_err(|err| WebhookError::InvalidJson(err.to_string()))
    }
    Ok(match event {
        Event::Push => Payload::Push(read(body)?),
        Event::Issues => Payload::Issues(read(body)?),
        Event::IssueComment => Payload::IssueComment(read(body)?),
        Event::PullRequest => Payload::PullRequest(read(body)?),
        Event::PullRequestReview => Payload::PullRequestReview(read(body)?),
        Event::PullRequestReviewComment => Payload::PullRequestReviewComment(read(body)?),
        Event::Release => Payload::Release(read(body)?),
        Event::Installation => Payload::Installation(read(body)?),
        Event::InstallationRepositories => Payload::InstallationRepositories(read(body)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SECRET: &[u8] = b"it's-a-secret";
    const BODY: &[u8] = br#"{"action":"opened","number":7}"#;

    fn sign(secret: &[u8], body: &[u8]) -> String {
        let mut mac = HmacSha256::new_from_slice(secret).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(&mac.finalize().into_bytes()))
    }

    #[test]
    fn valid_signature_verifies() {
        assert_eq!(verify(SECRET, BODY, &sign(SECRET, BODY)), Ok(()));
    }

    #[test]
    fn uppercase_hex_digest_is_accepted() {
        let header = sign(SECRET, BODY)
            .to_ascii_uppercase()
            .replacen("SHA256", "sha256", 1);
        assert_eq!(verify(SECRET, BODY, &header), Ok(()));
    }

    #[test]
    fn wrong_secret_is_a_mismatch() {
        assert_eq!(
            verify(b"other", BODY, &sign(SECRET, BODY)),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn tampered_body_is_a_mismatch() {
        let header = sign(SECRET, BODY);
        assert_eq!(
            verify(SECRET, br#"{"action":"closed","number":7}"#, &header),
            Err(VerifyError::Mismatch)
        );
    }

    #[test]
    fn missing_or_blank_header_is_refused() {
        assert_eq!(verify(SECRET, BODY, ""), Err(VerifyError::MissingSignature));
        assert_eq!(
            verify(SECRET, BODY, "   "),
            Err(VerifyError::MissingSignature)
        );
    }

    #[test]
    fn other_algorithms_are_refused_even_when_well_formed() {
        let sha1 = format!("sha1={}", "0".repeat(40));
        assert_eq!(
            verify(SECRET, BODY, &sha1),
            Err(VerifyError::UnsupportedAlgorithm)
        );
    }

    #[test]
    fn malformed_digests_are_refused() {
        assert_eq!(
            verify(SECRET, BODY, "deadbeef"),
            Err(VerifyError::MalformedSignature)
        );
        assert_eq!(
            verify(SECRET, BODY, "sha256=zz"),
            Err(VerifyError::MalformedSignature)
        );
        assert_eq!(
            verify(SECRET, BODY, &format!("sha256={}", "a".repeat(62))),
            Err(VerifyError::MalformedSignature)
        );
        assert_eq!(
            verify(SECRET, BODY, &format!("sha256=+{}", "a".repeat(63))),
            Err(VerifyError::MalformedSignature)
        );
    }

    #[test]
    fn empty_secret_never_verifies() {
        assert_eq!(
            verify(b"", BODY, &sign(b"", BODY)),
            Err(VerifyError::EmptySecret)
        );
    }

    #[test]
    fn delivery_reads_envelope_and_event() {
        let body = json!({
            "installation": {"id": 4242},
            "repository": {"full_name": "EarthOS-world/EarthOS.world"},
            "action": "opened"
        })
        .to_string();
        let delivery = Delivery::parse(
            Some("pull_request"),
            Some("72d3162e-cc78-11e3-81ab-4c9d3741cb6e"),
            body.as_bytes(),
        )
        .unwrap();
        assert_eq!(delivery.event, Event::PullRequest);
        assert_eq!(delivery.installation_id, Some(4242));
        assert_eq!(
            delivery.repo_full_name.as_deref(),
            Some("EarthOS-world/EarthOS.world")
        );
        assert_eq!(delivery.id, "72d3162e-cc78-11e3-81ab-4c9d3741cb6e");
    }

    #[test]
    fn delivery_refuses_unlisted_events_and_missing_headers() {
        assert_eq!(
            Delivery::parse(Some("ping"), Some("id-1"), b"{}"),
            Err(WebhookError::UnsupportedEvent("ping".to_owned()))
        );
        assert_eq!(
            Delivery::parse(None, Some("id-1"), b"{}"),
            Err(WebhookError::MissingHeader(EVENT_HEADER))
        );
        assert_eq!(
            Delivery::parse(Some("push"), None, b"{}"),
            Err(WebhookError::MissingHeader(DELIVERY_HEADER))
        );
    }

    #[test]
    fn delivery_refuses_bad_replay_keys() {
        let long = "x".repeat(MAX_DELIVERY_ID_LEN + 1);
        for id in ["", "has space", "tab\tid", long.as_str()] {
            assert_eq!(
                Delivery::parse(Some("push"), Some(id), b"{}"),
                Err(WebhookError::InvalidDeliveryId),
                "{id:?}"
            );
        }
    }

    #[test]
    fn delivery_refuses_bodies_that_are_not_json() {
        assert!(matches!(
            Delivery::parse(Some("push"), Some("id-1"), b"not json"),
            Err(WebhookError::InvalidJson(_))
        ));
    }

    #[test]
    fn every_listed_event_round_trips_its_header_name() {
        for event in [
            Event::Push,
            Event::Issues,
            Event::IssueComment,
            Event::PullRequest,
            Event::PullRequestReview,
            Event::PullRequestReviewComment,
            Event::Release,
            Event::Installation,
            Event::InstallationRepositories,
        ] {
            assert_eq!(Event::from_header(event.as_str()), Some(event));
        }
    }

    #[test]
    fn push_payload_drops_the_author_email() {
        let body = json!({
            "ref": "refs/heads/main", "before": "a", "after": "b",
            "repository": {"full_name": "o/r"},
            "commits": [{
                "id": "b", "message": "m", "timestamp": "t",
                "author": {"name": "N", "email": "leak@example.com"}
            }]
        })
        .to_string();
        let Payload::Push(push) = parse_payload(Event::Push, body.as_bytes()).unwrap() else {
            panic!("push");
        };
        assert_eq!(push.git_ref, "refs/heads/main");
        assert_eq!(push.commits.len(), 1);
        assert!(!format!("{push:?}").contains("leak@example.com"));
    }

    #[test]
    fn pull_request_payload_reads_the_item() {
        let body = json!({
            "action": "closed", "number": 9,
            "pull_request": {
                "number": 9, "title": "T", "body": null, "state": "closed",
                "html_url": "https://github.com/o/r/pull/9", "user": {"login": "alice"},
                "labels": [{"name": "bug"}], "created_at": "c", "updated_at": "u",
                "closed_at": "x", "merged_at": "m", "comments": 2
            },
            "repository": {"full_name": "o/r"}
        })
        .to_string();
        let Payload::PullRequest(pr) = parse_payload(Event::PullRequest, body.as_bytes()).unwrap()
        else {
            panic!("pull_request");
        };
        assert_eq!(pr.number, 9);
        assert_eq!(pr.pull_request.merged_at(), Some("m"));
        assert_eq!(pr.pull_request.labels[0].name, "bug");
    }

    #[test]
    fn release_and_installation_payloads_parse() {
        let release = json!({
            "action": "published",
            "release": {"id": 1, "tag_name": "v1", "name": null, "body": null,
                        "html_url": "h", "published_at": "p", "author": null},
            "repository": {"full_name": "o/r"}
        })
        .to_string();
        assert!(parse_payload(Event::Release, release.as_bytes()).is_ok());

        let installation = json!({
            "action": "created",
            "installation": {"id": 5, "account": {"login": "o"}},
            "repositories": [{"full_name": "o/r"}]
        })
        .to_string();
        let Payload::Installation(info) =
            parse_payload(Event::Installation, installation.as_bytes()).unwrap()
        else {
            panic!("installation");
        };
        assert_eq!(info.installation.id, 5);
        assert_eq!(info.repositories.len(), 1);

        let repos = json!({
            "action": "added", "installation": {"id": 5, "account": null},
            "repositories_added": [{"full_name": "o/r"}]
        })
        .to_string();
        assert!(parse_payload(Event::InstallationRepositories, repos.as_bytes()).is_ok());
    }

    #[test]
    fn payload_refuses_a_body_missing_required_fields() {
        assert!(matches!(
            parse_payload(Event::IssueComment, b"{\"action\":\"created\"}"),
            Err(WebhookError::InvalidJson(_))
        ));
    }
}
