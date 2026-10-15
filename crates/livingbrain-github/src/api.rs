//! Typed models of the GitHub REST responses this crate reads.
//!
//! Only the fields the mapping uses are modelled. serde ignores everything
//! else, so a field GitHub adds later is not a parse failure. Equally, a
//! field that must never reach the brain is simply never named here: no
//! struct in this crate has an `email`, so a commit author's address cannot
//! be read into memory, let alone written.

use serde::Deserialize;

/// A GitHub account, as the REST API names one by its login. Absent for
/// deleted or anonymous accounts, which is why every use is an `Option`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    pub login: String,
}

/// A label on an issue or pull request.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Label {
    pub name: String,
}

/// The git-level author of a commit: the display name and the date. The
/// address git also records is deliberately not modelled.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitAuthor {
    pub name: Option<String>,
    pub date: Option<String>,
}

/// The git-level commit object: the message and the author line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GitCommit {
    pub message: String,
    pub author: Option<GitAuthor>,
}

/// One entry of `GET /repos/{owner}/{repo}/commits`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommitSummary {
    pub sha: String,
    pub html_url: String,
    pub commit: GitCommit,
    pub author: Option<User>,
}

/// `GET /repos/{owner}/{repo}/commits/{sha}`: the summary plus the files the
/// commit touched. `patch` is present only when GitHub returned the diff for
/// that file, which it may omit for large or binary files.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommitDetail {
    pub sha: String,
    pub html_url: String,
    pub commit: GitCommit,
    pub author: Option<User>,
    #[serde(default)]
    pub files: Vec<CommitFile>,
    pub stats: Option<CommitStats>,
}

/// One file in a commit's `files` array.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CommitFile {
    pub filename: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    pub patch: Option<String>,
}

/// The `stats` object on a commit detail: the true totals, which the `files`
/// array can under-count when GitHub truncates it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct CommitStats {
    pub additions: u64,
    pub deletions: u64,
    pub total: u64,
}

/// The `pull_request` stub an issue carries when the issue is a pull request.
/// The listing puts `merged_at` here; a pull request object puts it at the
/// top level instead, which [`Item::merged_at`] reads either way.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullRequestRef {
    pub merged_at: Option<String>,
}

/// An issue or a pull request, as `GET /repos/{owner}/{repo}/issues` lists
/// it and as the issue and pull request webhook payloads carry it. A pull
/// request is an issue with `pull_request` set; the record mapping takes the
/// kind from the caller, not from this field, because a webhook pull request
/// object has no `pull_request` key of its own.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Item {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    pub html_url: String,
    pub user: Option<User>,
    #[serde(default)]
    pub labels: Vec<Label>,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    #[serde(default)]
    pub comments: u64,
    pub merged_at: Option<String>,
    pub pull_request: Option<PullRequestRef>,
}

impl Item {
    /// When a pull request was merged, from either place GitHub puts it.
    #[must_use]
    pub fn merged_at(&self) -> Option<&str> {
        self.merged_at.as_deref().or_else(|| {
            self.pull_request
                .as_ref()
                .and_then(|pr| pr.merged_at.as_deref())
        })
    }
}

/// One comment on an issue or a pull request conversation
/// (`GET .../issues/{n}/comments`, and the `issue_comment` payload).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    pub created_at: String,
    pub html_url: Option<String>,
}

/// One review on a pull request (`GET .../pulls/{n}/reviews`). A review that
/// is still pending has no `submitted_at`, and the mapping drops it: it is
/// not visible to anyone else yet.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct PullReview {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    pub state: String,
    pub submitted_at: Option<String>,
    pub html_url: Option<String>,
}

/// An inline review comment on a pull request diff (the
/// `pull_request_review_comment` payload).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReviewComment {
    pub id: u64,
    pub user: Option<User>,
    pub body: Option<String>,
    pub path: String,
    pub created_at: String,
    pub html_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_at_reads_listing_and_pull_request_shapes() {
        let listing: Item = serde_json::from_value(json!({
            "number": 1, "title": "t", "body": null, "state": "closed",
            "html_url": "u", "user": null, "created_at": "a", "updated_at": "b",
            "closed_at": "c", "pull_request": {"merged_at": "m1"}
        }))
        .unwrap();
        assert_eq!(listing.merged_at(), Some("m1"));

        let pr: Item = serde_json::from_value(json!({
            "number": 2, "title": "t", "body": null, "state": "closed",
            "html_url": "u", "user": null, "created_at": "a", "updated_at": "b",
            "closed_at": "c", "merged_at": "m2"
        }))
        .unwrap();
        assert_eq!(pr.merged_at(), Some("m2"));
        assert!(pr.pull_request.is_none());
    }

    #[test]
    fn commit_detail_ignores_the_email_field() {
        let detail: CommitDetail = serde_json::from_value(json!({
            "sha": "abc", "html_url": "u", "author": null,
            "commit": {"message": "m", "author": {"name": "N", "email": "secret@example.com", "date": "d"}}
        }))
        .unwrap();
        let author = detail.commit.author.unwrap();
        assert_eq!(author.name.as_deref(), Some("N"));
        assert!(!format!("{author:?}").contains("secret@example.com"));
    }
}
