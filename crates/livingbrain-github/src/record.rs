//! Mapping GitHub objects to the records the brain files, and the parts a
//! `SourceIngest` is built from.
//!
//! Three rules run through the whole module:
//!
//! - **No email, ever.** A commit author is a login and a display name; the
//!   record's `Person` has no address field, and the API models have none
//!   either. Commit messages are free text and may contain an address
//!   (`Signed-off-by`); those pass through here and the ingest pipeline's
//!   redaction is what catches them, as it does for every body.
//! - **Deterministic bytes.** The body is the record serialized with serde,
//!   whose field order is the struct's declaration order, and every list is
//!   sorted by a key the data carries. The same GitHub state therefore
//!   produces the same bytes, so `sha256(body)` is a stable identity and a
//!   re-sync that changes nothing dedupes to the same ledger row.
//! - **Nothing fetched-at.** No field records when this crate ran. A
//!   timestamp in a record is one GitHub stated.

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::api::{CommitDetail, IssueComment, Item, PullReview};
use crate::hex;

/// The record format version. Bumped when a field is added, renamed or
/// reordered, so a reader can tell two shapes apart.
pub const SCHEMA_VERSION: u32 = 1;

/// The longest project name, mirroring `livingbrain_pages::MAX_PROJECT_LEN`
/// (a repository name is project-sized; the ledger refuses a longer one).
const MAX_PROJECT_LEN: usize = 64;

/// The three kinds this connection files, as the ledger's `SourceKind` wire
/// names. The mapping to the ledger enum is the caller's, which keeps this
/// crate free of the pages module; a test pins these names to the ledger's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GithubKind {
    Commit,
    Issue,
    PullRequest,
}

impl GithubKind {
    /// The `SourceKind::as_str` wire name for this kind.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Self::Commit => "github_commit",
            Self::Issue => "github_issue",
            Self::PullRequest => "github_pr",
        }
    }
}

/// Whether a listed item is an issue or a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemKind {
    Issue,
    PullRequest,
}

/// Why a repository name was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    /// Not `owner/name`, or a segment is empty, `.`, `..`, or has a character
    /// outside `[A-Za-z0-9._-]`, or the name is not usable as a project.
    InvalidRepo(String),
}

/// A repository, `owner/name`. The `name` is the project the brain files its
/// records under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    owner: String,
    name: String,
}

impl RepoRef {
    /// Parses `owner/name`.
    ///
    /// # Errors
    ///
    /// [`RecordError::InvalidRepo`] for anything that is not two safe
    /// segments. The name must also be usable as a project: at most 64
    /// bytes, starting with a letter or digit.
    pub fn parse(full_name: &str) -> Result<Self, RecordError> {
        let invalid = || RecordError::InvalidRepo(full_name.to_owned());
        let (owner, name) = full_name.split_once('/').ok_or_else(invalid)?;
        if !is_segment(owner) || !is_segment(name) || name.len() > MAX_PROJECT_LEN {
            return Err(invalid());
        }
        if !name.as_bytes()[0].is_ascii_alphanumeric() {
            return Err(invalid());
        }
        Ok(Self {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }

    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The repository name without its owner. This is the project name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// `owner/name`, as the REST API and the webhooks spell it.
    #[must_use]
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

fn is_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A person as a record names one: the login GitHub knows them by, and the
/// display name git recorded. Never an address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Person {
    pub login: Option<String>,
    pub name: Option<String>,
}

/// One file a commit touched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileChange {
    pub path: String,
    pub status: String,
    pub additions: u64,
    pub deletions: u64,
    /// The unified diff for this file, present only when
    /// [`MapOptions::include_small_diffs_max_bytes`] allowed it.
    pub patch: Option<String>,
}

/// The totals a commit reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Totals {
    pub files_changed: u64,
    pub additions: u64,
    pub deletions: u64,
}

/// A commit, as filed. No full diff by default: the file list and the
/// totals say what changed, and a per-file patch is opt-in under a byte cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitRecord {
    pub schema: u32,
    pub kind: &'static str,
    pub repo: String,
    pub sha: String,
    pub origin_ref: String,
    pub message: String,
    pub author: Person,
    /// The git author date, as GitHub stated it.
    pub authored_at: Option<String>,
    pub files: Vec<FileChange>,
    pub totals: Totals,
}

/// One comment or review folded into an issue or pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Activity {
    pub kind: &'static str,
    /// When it happened: the comment's creation, the review's submission.
    pub at: String,
    pub author: Option<String>,
    pub body: Option<String>,
    /// The review state (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`); `None`
    /// for a comment.
    pub state: Option<String>,
    pub origin_ref: Option<String>,
}

/// An issue or pull request, with its comments and reviews in the order they
/// happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ItemRecord {
    pub schema: u32,
    pub kind: &'static str,
    pub repo: String,
    pub number: u64,
    pub origin_ref: String,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub activity: Vec<Activity>,
}

/// Options for mapping a commit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MapOptions {
    /// When set, a file's patch is kept if it is at most this many bytes.
    /// `None` (the default) keeps no patch at all: the file list and totals
    /// are the record, and a large diff is never stored by default.
    pub include_small_diffs_max_bytes: Option<usize>,
}

/// Maps a commit detail to its record.
#[must_use]
pub fn map_commit(repo: &RepoRef, detail: &CommitDetail, options: MapOptions) -> CommitRecord {
    let files: Vec<FileChange> = detail
        .files
        .iter()
        .map(|file| FileChange {
            path: file.filename.clone(),
            status: file.status.clone(),
            additions: file.additions,
            deletions: file.deletions,
            patch: options.include_small_diffs_max_bytes.and_then(|cap| {
                file.patch
                    .as_ref()
                    .filter(|patch| patch.len() <= cap)
                    .cloned()
            }),
        })
        .collect();
    let totals = match detail.stats {
        Some(stats) => Totals {
            files_changed: u64::try_from(files.len()).unwrap_or(u64::MAX),
            additions: stats.additions,
            deletions: stats.deletions,
        },
        None => Totals {
            files_changed: u64::try_from(files.len()).unwrap_or(u64::MAX),
            additions: files.iter().map(|f| f.additions).sum(),
            deletions: files.iter().map(|f| f.deletions).sum(),
        },
    };
    CommitRecord {
        schema: SCHEMA_VERSION,
        kind: "commit",
        repo: repo.full_name(),
        sha: detail.sha.clone(),
        origin_ref: detail.html_url.clone(),
        message: detail.commit.message.clone(),
        author: Person {
            login: detail.author.as_ref().map(|user| user.login.clone()),
            name: detail.commit.author.as_ref().and_then(|a| a.name.clone()),
        },
        authored_at: detail.commit.author.as_ref().and_then(|a| a.date.clone()),
        files,
        totals,
    }
}

/// Maps an issue or pull request, with the comments and reviews gathered for
/// it. A review with no `submitted_at` is still pending and is dropped.
/// Labels are sorted; activity is sorted by time, then kind, then origin, so
/// the order never depends on the order pages arrived in.
#[must_use]
pub fn map_item(
    repo: &RepoRef,
    item: &Item,
    kind: ItemKind,
    comments: &[IssueComment],
    reviews: &[PullReview],
) -> ItemRecord {
    let mut activity: Vec<Activity> = comments
        .iter()
        .map(|comment| Activity {
            kind: "comment",
            at: comment.created_at.clone(),
            author: comment.user.as_ref().map(|user| user.login.clone()),
            body: comment.body.clone(),
            state: None,
            origin_ref: comment.html_url.clone(),
        })
        .collect();
    activity.extend(reviews.iter().filter_map(|review| {
        Some(Activity {
            kind: "review",
            at: review.submitted_at.clone()?,
            author: review.user.as_ref().map(|user| user.login.clone()),
            body: review.body.clone(),
            state: Some(review.state.clone()),
            origin_ref: review.html_url.clone(),
        })
    }));
    // Time first, then kind and origin as tie-breaks: the order is what
    // happened, not the order the pages happened to arrive in.
    activity.sort_by(|a, b| (&a.at, a.kind, &a.origin_ref).cmp(&(&b.at, b.kind, &b.origin_ref)));

    let mut labels: Vec<String> = item.labels.iter().map(|label| label.name.clone()).collect();
    labels.sort();

    ItemRecord {
        schema: SCHEMA_VERSION,
        kind: match kind {
            ItemKind::Issue => "issue",
            ItemKind::PullRequest => "pull_request",
        },
        repo: repo.full_name(),
        number: item.number,
        origin_ref: item.html_url.clone(),
        title: item.title.clone(),
        body: item.body.clone(),
        state: item.state.clone(),
        author: item.user.as_ref().map(|user| user.login.clone()),
        labels,
        created_at: item.created_at.clone(),
        updated_at: item.updated_at.clone(),
        closed_at: item.closed_at.clone(),
        merged_at: item.merged_at().map(str::to_owned),
        activity,
    }
}

/// The record serialized as JSON. Field order is the struct's declaration
/// order, so this string is the same for the same record every time.
#[must_use]
pub fn canonical_json<T: Serialize>(record: &T) -> String {
    // A record is plain data with string keys; serialization cannot fail.
    serde_json::to_string(record).unwrap_or_default()
}

/// Lowercase hex SHA-256 of `body`: the ledger's identity for a source.
#[must_use]
pub fn body_sha256(body: &str) -> String {
    hex::encode(&Sha256::digest(body.as_bytes()))
}

/// The parts a `SourceIngest` is built from, minus the workspace and the
/// importing member, which the caller knows and this crate does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceParts {
    pub kind: GithubKind,
    /// The project the source is filed under: the repository name.
    pub project: String,
    /// `github/<owner>/<repo>/commit/<sha>`, `.../issue/<n>` or `.../pr/<n>`.
    pub rel_path: String,
    /// The permalink GitHub gave.
    pub origin_ref: String,
    /// The canonical JSON of the record. Redacted by the ingest pipeline, not
    /// here.
    pub body: String,
    /// [`body_sha256`] of `body`.
    pub body_sha256: String,
}

fn parts(
    kind: GithubKind,
    repo: &RepoRef,
    leaf: String,
    origin_ref: &str,
    body: String,
) -> SourceParts {
    let body_sha256 = body_sha256(&body);
    SourceParts {
        kind,
        project: repo.name().to_owned(),
        rel_path: format!("github/{}/{}/{leaf}", repo.owner(), repo.name()),
        origin_ref: origin_ref.to_owned(),
        body,
        body_sha256,
    }
}

/// The source parts for a commit record.
#[must_use]
pub fn commit_source(repo: &RepoRef, record: &CommitRecord) -> SourceParts {
    parts(
        GithubKind::Commit,
        repo,
        format!("commit/{}", record.sha),
        &record.origin_ref,
        canonical_json(record),
    )
}

/// The source parts for an issue or pull request record.
#[must_use]
pub fn item_source(repo: &RepoRef, record: &ItemRecord) -> SourceParts {
    let (kind, leaf) = match record.kind {
        "pull_request" => (GithubKind::PullRequest, format!("pr/{}", record.number)),
        _ => (GithubKind::Issue, format!("issue/{}", record.number)),
    };
    parts(kind, repo, leaf, &record.origin_ref, canonical_json(record))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo() -> RepoRef {
        RepoRef::parse("EarthOS-world/EarthOS.world").unwrap()
    }

    fn commit_detail(with_patch: bool) -> CommitDetail {
        serde_json::from_value(json!({
            "sha": "9f2c1e0000000000000000000000000000000000",
            "html_url": "https://github.com/EarthOS-world/EarthOS.world/commit/9f2c1e",
            "author": {"login": "alice"},
            "commit": {
                "message": "fix: tiles\n\nSigned-off-by: Alice <alice@example.com>",
                "author": {"name": "Alice A", "email": "alice@example.com", "date": "2026-01-02T03:04:05Z"}
            },
            "files": [
                {"filename": "src/a.rs", "status": "modified", "additions": 3, "deletions": 1,
                 "patch": if with_patch { json!("@@ -1 +1 @@\n-a\n+b") } else { json!(null) }},
                {"filename": "big.bin", "status": "added", "additions": 500, "deletions": 0,
                 "patch": "x".repeat(10_000)}
            ],
            "stats": {"additions": 503, "deletions": 1, "total": 504}
        }))
        .unwrap()
    }

    #[test]
    fn repo_parse_accepts_real_names_and_refuses_unsafe_ones() {
        let ok = RepoRef::parse("EarthOS-world/EarthOS.world").unwrap();
        assert_eq!(ok.name(), "EarthOS.world");
        assert_eq!(ok.full_name(), "EarthOS-world/EarthOS.world");
        for bad in [
            "",
            "noslash",
            "a/b/c",
            "/name",
            "owner/",
            "../x",
            "owner/..",
            "owner/.hidden",
            "owner/with space",
            "owner/with\u{0}nul",
        ] {
            assert!(RepoRef::parse(bad).is_err(), "{bad:?} should be refused");
        }
        let long = format!("o/{}", "a".repeat(65));
        assert!(RepoRef::parse(&long).is_err());
    }

    #[test]
    fn commit_mapping_drops_every_email() {
        let record = map_commit(&repo(), &commit_detail(true), MapOptions::default());
        let json = canonical_json(&record);
        // The author is structured and carries no address. The message is
        // free text, kept as written (redaction is the ingest pipeline's job,
        // and it runs on the same body), so only the rest of the record is
        // held to the no-address rule.
        assert!(!canonical_json(&record.author).contains('@'));
        assert!(
            !json
                .replace(&canonical_json(&record.message), "")
                .contains('@')
        );
        assert!(!json.contains("\"email\""));
        assert!(record.message.contains("Signed-off-by"));
        assert_eq!(record.author.login.as_deref(), Some("alice"));
        assert_eq!(record.author.name.as_deref(), Some("Alice A"));
    }

    #[test]
    fn commit_patches_are_off_by_default_and_capped_when_asked() {
        let default = map_commit(&repo(), &commit_detail(true), MapOptions::default());
        assert!(default.files.iter().all(|f| f.patch.is_none()));

        let capped = map_commit(
            &repo(),
            &commit_detail(true),
            MapOptions {
                include_small_diffs_max_bytes: Some(100),
            },
        );
        assert_eq!(
            capped.files[0].patch.as_deref(),
            Some("@@ -1 +1 @@\n-a\n+b")
        );
        assert!(
            capped.files[1].patch.is_none(),
            "10 KB patch is over the cap"
        );
    }

    #[test]
    fn commit_totals_come_from_stats_when_present() {
        let record = map_commit(&repo(), &commit_detail(false), MapOptions::default());
        assert_eq!(
            record.totals,
            Totals {
                files_changed: 2,
                additions: 503,
                deletions: 1
            }
        );
    }

    #[test]
    fn serialization_is_deterministic_and_field_ordered() {
        let a = map_commit(&repo(), &commit_detail(true), MapOptions::default());
        let b = map_commit(&repo(), &commit_detail(true), MapOptions::default());
        let json = canonical_json(&a);
        assert_eq!(json, canonical_json(&b));
        assert!(
            json.starts_with(
                r#"{"schema":1,"kind":"commit","repo":"EarthOS-world/EarthOS.world","#
            )
        );
    }

    fn item(comments: u64, merged: bool) -> Item {
        serde_json::from_value(json!({
            "number": 12, "title": "Add globe", "body": "details",
            "state": "closed", "html_url": "https://github.com/o/r/pull/12",
            "user": {"login": "bob"}, "labels": [{"name": "zeta"}, {"name": "alpha"}],
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-05T00:00:00Z",
            "closed_at": "2026-01-05T00:00:00Z", "comments": comments,
            "pull_request": {"merged_at": if merged { json!("2026-01-05T00:00:00Z") } else { json!(null) }}
        }))
        .unwrap()
    }

    fn comment(at: &str, body: &str, id: u64) -> IssueComment {
        serde_json::from_value(json!({
            "id": id, "user": {"login": "carol"}, "body": body,
            "created_at": at, "html_url": format!("https://github.com/o/r/pull/12#c{id}")
        }))
        .unwrap()
    }

    fn review(submitted: Option<&str>, state: &str, id: u64) -> PullReview {
        serde_json::from_value(json!({
            "id": id, "user": {"login": "dan"}, "body": "ok", "state": state,
            "submitted_at": submitted, "html_url": format!("https://github.com/o/r/pull/12#r{id}")
        }))
        .unwrap()
    }

    #[test]
    fn item_mapping_folds_comments_and_reviews_chronologically() {
        let comments = [
            comment("2026-01-03T00:00:00Z", "later", 2),
            comment("2026-01-02T00:00:00Z", "first", 1),
        ];
        let reviews = [
            review(Some("2026-01-02T12:00:00Z"), "APPROVED", 5),
            review(None, "PENDING", 6),
        ];
        let record = map_item(
            &repo(),
            &item(2, true),
            ItemKind::PullRequest,
            &comments,
            &reviews,
        );
        assert_eq!(record.kind, "pull_request");
        assert_eq!(record.labels, vec!["alpha", "zeta"]);
        assert_eq!(record.merged_at.as_deref(), Some("2026-01-05T00:00:00Z"));
        let order: Vec<(&str, &str)> = record
            .activity
            .iter()
            .map(|a| (a.kind, a.at.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("comment", "2026-01-02T00:00:00Z"),
                ("review", "2026-01-02T12:00:00Z"),
                ("comment", "2026-01-03T00:00:00Z"),
            ],
            "the pending review is dropped"
        );
    }

    #[test]
    fn item_mapping_is_independent_of_arrival_order() {
        let c1 = comment("2026-01-02T00:00:00Z", "a", 1);
        let c2 = comment("2026-01-03T00:00:00Z", "b", 2);
        let forward = map_item(
            &repo(),
            &item(2, false),
            ItemKind::Issue,
            &[c1.clone(), c2.clone()],
            &[],
        );
        let backward = map_item(&repo(), &item(2, false), ItemKind::Issue, &[c2, c1], &[]);
        assert_eq!(canonical_json(&forward), canonical_json(&backward));
    }

    #[test]
    fn source_parts_carry_project_path_origin_and_digest() {
        let commit = map_commit(&repo(), &commit_detail(false), MapOptions::default());
        let parts = commit_source(&repo(), &commit);
        assert_eq!(parts.kind, GithubKind::Commit);
        assert_eq!(parts.project, "EarthOS.world");
        assert_eq!(
            parts.rel_path,
            "github/EarthOS-world/EarthOS.world/commit/9f2c1e0000000000000000000000000000000000"
        );
        assert_eq!(parts.origin_ref, commit.origin_ref);
        assert_eq!(parts.body_sha256.len(), 64);
        assert_eq!(parts.body_sha256, body_sha256(&parts.body));

        let issue = map_item(&repo(), &item(0, false), ItemKind::Issue, &[], &[]);
        let issue_parts = item_source(&repo(), &issue);
        assert_eq!(issue_parts.kind, GithubKind::Issue);
        assert!(issue_parts.rel_path.ends_with("/issue/12"));

        let pr = map_item(&repo(), &item(0, true), ItemKind::PullRequest, &[], &[]);
        let pr_parts = item_source(&repo(), &pr);
        assert_eq!(pr_parts.kind, GithubKind::PullRequest);
        assert!(pr_parts.rel_path.ends_with("/pr/12"));
    }

    #[test]
    fn same_state_same_digest_and_changed_state_new_digest() {
        let a = item_source(
            &repo(),
            &map_item(&repo(), &item(0, false), ItemKind::Issue, &[], &[]),
        );
        let b = item_source(
            &repo(),
            &map_item(&repo(), &item(0, false), ItemKind::Issue, &[], &[]),
        );
        assert_eq!(a.body_sha256, b.body_sha256);
        let c = [comment("2026-01-02T00:00:00Z", "new", 9)];
        let changed = item_source(
            &repo(),
            &map_item(&repo(), &item(1, false), ItemKind::Issue, &c, &[]),
        );
        assert_ne!(a.body_sha256, changed.body_sha256);
    }

    #[test]
    fn wire_names_match_the_ledger_kind_names() {
        assert_eq!(GithubKind::Commit.wire_name(), "github_commit");
        assert_eq!(GithubKind::Issue.wire_name(), "github_issue");
        assert_eq!(GithubKind::PullRequest.wire_name(), "github_pr");
    }
}
