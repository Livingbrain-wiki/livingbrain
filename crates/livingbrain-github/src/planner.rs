//! The backfill planner: which GitHub requests to make next, and how a
//! response moves the cursor. Pure: no clock, no HTTP, no storage.
//!
//! It runs only inside the Worker, driven by its cron or queue, which holds
//! the installation token and writes the ledger. Nothing here is a script,
//! an importer or a CLI command, and no test here talks to GitHub; the tests
//! drive the planner with fixtures.
//!
//! The loop a driver runs, once per tick:
//!
//! 1. `plan(&state, DEFAULT_BUDGET)` gives at most that many requests.
//! 2. For each request, the driver makes the call and, on success, calls
//!    [`apply`] with the request and the parsed response. A response whose
//!    kind does not match its request is refused, not applied.
//! 3. `Applied::finished_items` names the items whose comments and reviews
//!    are all in. The driver keeps each item from its listing response until
//!    it appears there, then maps it with [`crate::record::map_item`].
//! 4. The new `state` is persisted before the tick ends, so a crash resumes
//!    at the same cursor and re-fetches at most one page.
//!
//! The state is plain data and serializes; a tick that resumes from a stored
//! state plans exactly what the interrupted one would have.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

use crate::api::{CommitDetail, CommitSummary, IssueComment, Item, PullReview};
use crate::record::RepoRef;

/// Requests per tick. Each request is one API call; a tick of 45 stays well
/// inside the installation's hourly limit with room for webhook-driven work.
pub const DEFAULT_BUDGET: u32 = 45;

/// The page size asked for on every list. GitHub's maximum.
pub const PER_PAGE: u32 = 100;

/// Where a paginated listing stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cursor {
    /// Fetch this page next.
    Next(u32),
    /// Every page has been read.
    Done,
}

/// A per-item job: the comments and reviews still to read for one issue or
/// pull request. `Some(page)` is the next page of that list; `None` means it
/// is finished or was never needed (no comments, or an issue has no reviews).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemJob {
    pub number: u64,
    pub pull_request: bool,
    pub comments: Option<u32>,
    pub reviews: Option<u32>,
}

/// Everything a run needs to resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackfillState {
    /// The commit listing, newest first.
    pub commits: Cursor,
    /// The issue-and-pull-request listing, oldest update first, so the
    /// watermark only moves forward.
    pub issues: Cursor,
    /// Passed as `since` to the issue listing. `None` is a full backfill.
    pub since: Option<String>,
    /// The newest `updated_at` seen this run; becomes `since` at the next run.
    pub watermark: Option<String>,
    /// The commit listing stops at this sha, the newest one a previous run
    /// stored. `None` is a full backfill.
    pub stop_at_sha: Option<String>,
    /// The first commit this run listed; becomes `stop_at_sha` next run.
    pub newest_sha: Option<String>,
    /// Commits whose detail (files and totals) is still to fetch.
    pub commit_queue: VecDeque<String>,
    /// Items whose comments or reviews are still to fetch, in listing order.
    pub item_queue: VecDeque<ItemJob>,
}

impl Default for BackfillState {
    fn default() -> Self {
        Self {
            commits: Cursor::Next(1),
            issues: Cursor::Next(1),
            since: None,
            watermark: None,
            stop_at_sha: None,
            newest_sha: None,
            commit_queue: VecDeque::new(),
            item_queue: VecDeque::new(),
        }
    }
}

impl BackfillState {
    /// A fresh full backfill.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// True when the run has read everything it planned to: both listings are
    /// done and no item or commit is waiting.
    #[must_use]
    pub fn is_done(&self) -> bool {
        self.commits == Cursor::Done
            && self.issues == Cursor::Done
            && self.commit_queue.is_empty()
            && self.item_queue.is_empty()
    }

    /// The state for the next incremental run, once this one is done: the
    /// listings restart, the issue listing starts at the watermark, and the
    /// commit listing stops at the newest commit this run saw. If this run
    /// saw no issue update, the previous `since` is kept.
    #[must_use]
    pub fn next_run(&self) -> Self {
        Self {
            commits: Cursor::Next(1),
            issues: Cursor::Next(1),
            since: self.watermark.clone().or_else(|| self.since.clone()),
            watermark: None,
            stop_at_sha: self.newest_sha.clone().or_else(|| self.stop_at_sha.clone()),
            newest_sha: None,
            commit_queue: VecDeque::new(),
            item_queue: VecDeque::new(),
        }
    }
}

/// One GitHub call the planner wants made. Paths are relative to the REST
/// base, so the driver adds its host and auth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    CommitList { page: u32 },
    CommitDetail { sha: String },
    IssueList { page: u32, since: Option<String> },
    IssueComments { number: u64, page: u32 },
    PullReviews { number: u64, page: u32 },
}

impl Request {
    /// The path and query for this request against `repo`.
    #[must_use]
    pub fn path(&self, repo: &RepoRef) -> String {
        let base = format!("/repos/{}/{}", repo.owner(), repo.name());
        match self {
            Self::CommitList { page } => {
                format!("{base}/commits?per_page={PER_PAGE}&page={page}")
            }
            Self::CommitDetail { sha } => format!("{base}/commits/{sha}"),
            Self::IssueList { page, since } => {
                let mut path = format!(
                    "{base}/issues?state=all&sort=updated&direction=asc&per_page={PER_PAGE}&page={page}"
                );
                if let Some(since) = since {
                    path.push_str("&since=");
                    path.push_str(since);
                }
                path
            }
            Self::IssueComments { number, page } => {
                format!("{base}/issues/{number}/comments?per_page={PER_PAGE}&page={page}")
            }
            Self::PullReviews { number, page } => {
                format!("{base}/pulls/{number}/reviews?per_page={PER_PAGE}&page={page}")
            }
        }
    }
}

/// A parsed, successful response. `next_page` is what the `Link` header said
/// (see [`parse_next_page`]); `None` is the last page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    CommitList {
        items: Vec<CommitSummary>,
        next_page: Option<u32>,
    },
    CommitDetail(CommitDetail),
    IssueList {
        items: Vec<Item>,
        next_page: Option<u32>,
    },
    IssueComments {
        number: u64,
        items: Vec<IssueComment>,
        next_page: Option<u32>,
    },
    PullReviews {
        number: u64,
        items: Vec<PullReview>,
        next_page: Option<u32>,
    },
}

/// Why a response was not applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The response's kind or number does not match the request.
    Mismatch,
    /// The request's page is not the cursor's page: a response for a page
    /// this run was not waiting for.
    OutOfOrder,
    /// No queued item has this number.
    UnknownItem(u64),
    /// No queued commit has this sha.
    UnknownCommit(String),
}

/// What applying a response produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub state: BackfillState,
    /// Items whose comments and reviews are now all read, so the driver can
    /// map and file them. Includes issues that needed no follow-up at all.
    pub finished_items: Vec<u64>,
}

/// Plans the next requests, up to `budget`.
///
/// Order within a tick: the commit listing page, the issue listing page, the
/// comments and reviews of queued items in listing order, then commit
/// details. The listings come first so the queues they fill are as long as
/// the work allows. Deterministic: the same state and budget give the same
/// requests.
#[must_use]
pub fn plan(state: &BackfillState, budget: u32) -> Vec<Request> {
    let budget = usize::try_from(budget).unwrap_or(usize::MAX);
    let mut out = Vec::new();
    if let Cursor::Next(page) = state.commits {
        out.push(Request::CommitList { page });
    }
    if let Cursor::Next(page) = state.issues {
        out.push(Request::IssueList {
            page,
            since: state.since.clone(),
        });
    }
    'items: for job in &state.item_queue {
        if let Some(page) = job.comments {
            if out.len() >= budget {
                break 'items;
            }
            out.push(Request::IssueComments {
                number: job.number,
                page,
            });
        }
        if let (true, Some(page)) = (job.pull_request, job.reviews) {
            if out.len() >= budget {
                break 'items;
            }
            out.push(Request::PullReviews {
                number: job.number,
                page,
            });
        }
    }
    for sha in &state.commit_queue {
        if out.len() >= budget {
            break;
        }
        out.push(Request::CommitDetail { sha: sha.clone() });
    }
    out.truncate(budget);
    out
}

/// Applies a successful response to the state it was planned from.
///
/// # Errors
///
/// [`PlanError`] when the response does not belong to the request or to the
/// state. Nothing is applied on error; the caller keeps the old state.
pub fn apply(
    mut state: BackfillState,
    request: &Request,
    response: &Response,
) -> Result<Applied, PlanError> {
    let mut finished_items = Vec::new();
    match (request, response) {
        (Request::CommitList { page }, Response::CommitList { items, next_page }) => {
            expect_cursor(state.commits, *page)?;
            if let (1, Some(first)) = (*page, items.first()) {
                state.newest_sha = Some(first.sha.clone());
            }
            let mut stopped = false;
            for item in items {
                if state.stop_at_sha.as_deref() == Some(item.sha.as_str()) {
                    stopped = true;
                    break;
                }
                state.commit_queue.push_back(item.sha.clone());
            }
            state.commits = match (stopped, next_page) {
                (false, Some(next)) => Cursor::Next(*next),
                _ => Cursor::Done,
            };
        }
        (Request::CommitDetail { sha }, Response::CommitDetail(detail)) => {
            if detail.sha != *sha {
                return Err(PlanError::Mismatch);
            }
            let Some(position) = state.commit_queue.iter().position(|queued| queued == sha) else {
                return Err(PlanError::UnknownCommit(sha.clone()));
            };
            state.commit_queue.remove(position);
        }
        (Request::IssueList { page, .. }, Response::IssueList { items, next_page }) => {
            expect_cursor(state.issues, *page)?;
            for item in items {
                // The listing is oldest-update-first, but compare anyway: the
                // watermark only ever moves forward.
                if state
                    .watermark
                    .as_ref()
                    .is_none_or(|w| item.updated_at > *w)
                {
                    state.watermark = Some(item.updated_at.clone());
                }
                let pull_request = item.pull_request.is_some();
                let job = ItemJob {
                    number: item.number,
                    pull_request,
                    comments: (item.comments > 0).then_some(1),
                    reviews: pull_request.then_some(1),
                };
                if job.comments.is_none() && job.reviews.is_none() {
                    finished_items.push(item.number);
                } else {
                    state.item_queue.push_back(job);
                }
            }
            state.issues = next_page.map_or(Cursor::Done, Cursor::Next);
        }
        (
            Request::IssueComments { number, page },
            Response::IssueComments {
                number: got,
                next_page,
                ..
            },
        ) => {
            if got != number {
                return Err(PlanError::Mismatch);
            }
            let position = job_position(&state, *number)?;
            let job = &mut state.item_queue[position];
            if job.comments != Some(*page) {
                return Err(PlanError::OutOfOrder);
            }
            job.comments = *next_page;
            settle_job(&mut state, position, &mut finished_items);
        }
        (
            Request::PullReviews { number, page },
            Response::PullReviews {
                number: got,
                next_page,
                ..
            },
        ) => {
            if got != number {
                return Err(PlanError::Mismatch);
            }
            let position = job_position(&state, *number)?;
            let job = &mut state.item_queue[position];
            if job.reviews != Some(*page) {
                return Err(PlanError::OutOfOrder);
            }
            job.reviews = *next_page;
            settle_job(&mut state, position, &mut finished_items);
        }
        _ => return Err(PlanError::Mismatch),
    }
    Ok(Applied {
        state,
        finished_items,
    })
}

fn expect_cursor(cursor: Cursor, page: u32) -> Result<(), PlanError> {
    match cursor {
        Cursor::Next(expected) if expected == page => Ok(()),
        _ => Err(PlanError::OutOfOrder),
    }
}

fn job_position(state: &BackfillState, number: u64) -> Result<usize, PlanError> {
    state
        .item_queue
        .iter()
        .position(|job| job.number == number)
        .ok_or(PlanError::UnknownItem(number))
}

/// Removes a job once neither list has a page left, and reports it.
fn settle_job(state: &mut BackfillState, position: usize, finished: &mut Vec<u64>) {
    let job = &state.item_queue[position];
    if job.comments.is_none() && job.reviews.is_none() {
        finished.push(job.number);
        state.item_queue.remove(position);
    }
}

/// The next page named by a `Link` header, if it names one. Reads the
/// `rel="next"` entry and its `page=` query value; anything else is `None`.
#[must_use]
pub fn parse_next_page(link: &str) -> Option<u32> {
    link.split(',').find_map(|part| {
        if !part.contains("rel=\"next\"") {
            return None;
        }
        let url = part.split_once('<')?.1.split_once('>')?.0;
        let query = url.split_once('?')?.1;
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("page="))
            .and_then(|page| page.parse().ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repo() -> RepoRef {
        RepoRef::parse("EarthOS-world/EarthOS.world").unwrap()
    }

    /// A deterministic GitHub-shaped fixture: `commits` commits, and `items`
    /// listed items of which every fourth is a pull request. Every third item
    /// has two comments; every pull request has one review page.
    struct Fixture {
        commits: usize,
        items: usize,
    }

    impl Fixture {
        fn commit_page(&self, page: u32) -> Response {
            let per = PER_PAGE as usize;
            let start = (page as usize - 1) * per;
            let end = (start + per).min(self.commits);
            let items = (start..end)
                .map(|i| {
                    serde_json::from_value(json!({
                        "sha": format!("{:040x}", self.commits - i),
                        "html_url": "u",
                        "commit": {"message": "m", "author": null},
                        "author": null
                    }))
                    .unwrap()
                })
                .collect();
            let next_page = (end < self.commits).then_some(page + 1);
            Response::CommitList { items, next_page }
        }

        fn item(&self, n: usize) -> Item {
            let pr = n.is_multiple_of(4);
            let comments = if n.is_multiple_of(3) { 2 } else { 0 };
            serde_json::from_value(json!({
                "number": n + 1, "title": "t", "body": null, "state": "open",
                "html_url": "u", "user": null,
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": format!("2026-02-{:02}T00:00:00Z", (n % 27) + 1),
                "closed_at": null, "comments": comments,
                "pull_request": if pr { json!({"merged_at": null}) } else { json!(null) }
            }))
            .unwrap()
        }

        fn issue_page(&self, page: u32) -> Response {
            let per = PER_PAGE as usize;
            let start = (page as usize - 1) * per;
            let end = (start + per).min(self.items);
            let items = (start..end).map(|i| self.item(i)).collect();
            let next_page = (end < self.items).then_some(page + 1);
            Response::IssueList { items, next_page }
        }

        fn respond(&self, request: &Request) -> Response {
            match request {
                Request::CommitList { page } => self.commit_page(*page),
                Request::CommitDetail { sha } => serde_json::from_value(json!({
                    "sha": sha, "html_url": "u", "author": null,
                    "commit": {"message": "m", "author": null}, "files": []
                }))
                .map(Response::CommitDetail)
                .unwrap(),
                Request::IssueList { page, .. } => self.issue_page(*page),
                Request::IssueComments { number, page } => Response::IssueComments {
                    number: *number,
                    items: Vec::new(),
                    next_page: (*page < 2 && (number - 1).is_multiple_of(3)).then_some(page + 1),
                },
                Request::PullReviews { number, .. } => Response::PullReviews {
                    number: *number,
                    items: Vec::new(),
                    next_page: None,
                },
            }
        }
    }

    /// Runs ticks until done. Returns (ticks, requests, finished items).
    fn drive(
        fixture: &Fixture,
        mut state: BackfillState,
    ) -> (usize, usize, Vec<u64>, BackfillState) {
        let (mut ticks, mut total, mut finished) = (0, 0, Vec::new());
        while !state.is_done() {
            let requests = plan(&state, DEFAULT_BUDGET);
            assert!(!requests.is_empty(), "stalled with state {state:?}");
            assert!(requests.len() <= DEFAULT_BUDGET as usize);
            ticks += 1;
            total += requests.len();
            for request in &requests {
                let response = fixture.respond(request);
                let applied = apply(state, request, &response).unwrap();
                state = applied.state;
                finished.extend(applied.finished_items);
            }
            assert!(ticks < 10_000, "did not converge");
        }
        (ticks, total, finished, state)
    }

    #[test]
    fn plan_never_exceeds_the_budget() {
        let mut state = BackfillState::new();
        state.commit_queue = (0..200).map(|i| format!("{i:040x}")).collect();
        state.item_queue = (0..200)
            .map(|n| ItemJob {
                number: n,
                pull_request: true,
                comments: Some(1),
                reviews: Some(1),
            })
            .collect();
        for budget in [0, 1, 2, 45] {
            assert!(
                plan(&state, budget).len() <= budget as usize,
                "budget {budget}"
            );
        }
        assert_eq!(plan(&state, 45).len(), 45);
    }

    #[test]
    fn listings_are_planned_before_item_and_commit_work() {
        let state = BackfillState::new();
        let requests = plan(&state, DEFAULT_BUDGET);
        assert_eq!(
            requests,
            vec![
                Request::CommitList { page: 1 },
                Request::IssueList {
                    page: 1,
                    since: None
                },
            ]
        );
    }

    #[test]
    fn a_response_for_the_wrong_request_is_refused() {
        let state = BackfillState::new();
        let fixture = Fixture {
            commits: 1,
            items: 1,
        };
        let wrong = fixture.issue_page(1);
        assert_eq!(
            apply(state.clone(), &Request::CommitList { page: 1 }, &wrong),
            Err(PlanError::Mismatch)
        );
        assert_eq!(
            apply(
                state,
                &Request::CommitList { page: 2 },
                &fixture.commit_page(1)
            ),
            Err(PlanError::OutOfOrder)
        );
    }

    #[test]
    fn a_stale_item_page_is_refused_not_applied() {
        let mut state = BackfillState::new();
        state.item_queue.push_back(ItemJob {
            number: 5,
            pull_request: false,
            comments: Some(2),
            reviews: None,
        });
        let response = Response::IssueComments {
            number: 5,
            items: vec![],
            next_page: None,
        };
        assert_eq!(
            apply(
                state.clone(),
                &Request::IssueComments { number: 5, page: 1 },
                &response
            ),
            Err(PlanError::OutOfOrder)
        );
        assert_eq!(
            apply(
                state,
                &Request::IssueComments { number: 9, page: 2 },
                &response
            ),
            Err(PlanError::Mismatch)
        );
    }

    #[test]
    fn resume_from_a_stored_state_plans_the_same_requests() {
        let fixture = Fixture {
            commits: 250,
            items: 300,
        };
        let mut state = BackfillState::new();
        for _ in 0..3 {
            for request in plan(&state, DEFAULT_BUDGET) {
                state = apply(state, &request, &fixture.respond(&request))
                    .unwrap()
                    .state;
            }
        }
        let stored = serde_json::to_string(&state).unwrap();
        let resumed: BackfillState = serde_json::from_str(&stored).unwrap();
        assert_eq!(plan(&state, DEFAULT_BUDGET), plan(&resumed, DEFAULT_BUDGET));
        assert_eq!(state, resumed);
    }

    #[test]
    fn a_fresh_run_reads_everything_and_reports_each_item_once() {
        let fixture = Fixture {
            commits: 250,
            items: 300,
        };
        let (_, _, finished, state) = drive(&fixture, BackfillState::new());
        assert!(state.is_done());
        let mut numbers = finished.clone();
        numbers.sort_unstable();
        numbers.dedup();
        assert_eq!(numbers.len(), finished.len(), "an item finished twice");
        assert_eq!(finished.len(), 300, "every listed item finishes");
    }

    #[test]
    fn incremental_run_stops_at_the_previous_newest_commit_and_uses_the_watermark() {
        let fixture = Fixture {
            commits: 10,
            items: 5,
        };
        let (_, _, _, done) = drive(&fixture, BackfillState::new());
        let next = done.next_run();
        assert_eq!(next.stop_at_sha, done.newest_sha);
        assert_eq!(next.since, done.watermark);

        // Three new commits land on top of the old head (sha 10 here). The
        // first listing page must queue exactly those three and stop there.
        let newer = Fixture {
            commits: 13,
            items: 5,
        };
        let request = plan(&next, DEFAULT_BUDGET).remove(0);
        let applied = apply(next, &request, &newer.respond(&request)).unwrap();
        let queued: Vec<String> = applied.state.commit_queue.iter().cloned().collect();
        let expected: Vec<String> = [13, 12, 11].iter().map(|n| format!("{n:040x}")).collect();
        assert_eq!(queued, expected);
        assert_eq!(applied.state.commits, Cursor::Done);
    }

    #[test]
    fn an_empty_repository_finishes_in_one_tick() {
        let fixture = Fixture {
            commits: 0,
            items: 0,
        };
        let (ticks, total, finished, state) = drive(&fixture, BackfillState::new());
        assert_eq!((ticks, total), (1, 2));
        assert!(finished.is_empty());
        assert!(state.is_done());
    }

    #[test]
    fn earthos_sized_backfill_fits_the_hourly_budget() {
        // 716 commits, 593 issues and pull requests (552 pull requests plus 41
        // open issues, listed in one stream). This is a synthetic shape for the
        // budget arithmetic, not data from GitHub.
        let fixture = Fixture {
            commits: 716,
            items: 593,
        };
        let (ticks, total, finished, state) = drive(&fixture, BackfillState::new());
        assert!(state.is_done());
        assert_eq!(finished.len(), 593);
        // Commit listing (8 pages) + one detail per commit + issue listing
        // (6 pages) + one comments/reviews request per item that needs it.
        assert!(total >= 716 + 8 + 6, "total {total}");
        assert!(
            total < 5_000,
            "a full backfill must not spend an hour of quota: {total}"
        );
        // Each tick is capped, so the backfill takes many ticks, never one big burst.
        assert!(ticks >= total.div_ceil(DEFAULT_BUDGET as usize));
    }

    #[test]
    fn link_header_next_page_is_read() {
        let link = "<https://api.github.com/repositories/1/commits?per_page=100&page=2>; rel=\"next\", \
                    <https://api.github.com/repositories/1/commits?per_page=100&page=8>; rel=\"last\"";
        assert_eq!(parse_next_page(link), Some(2));
        let last = "<https://api.github.com/x?page=1>; rel=\"prev\", <https://api.github.com/x?page=8>; rel=\"last\"";
        assert_eq!(parse_next_page(last), None);
        assert_eq!(parse_next_page(""), None);
    }

    #[test]
    fn request_paths_name_the_repo_and_the_cursor() {
        let r = repo();
        assert_eq!(
            Request::CommitList { page: 3 }.path(&r),
            "/repos/EarthOS-world/EarthOS.world/commits?per_page=100&page=3"
        );
        assert_eq!(
            Request::IssueList {
                page: 1,
                since: Some("2026-02-01T00:00:00Z".to_owned())
            }
            .path(&r),
            "/repos/EarthOS-world/EarthOS.world/issues?state=all&sort=updated&direction=asc&per_page=100&page=1&since=2026-02-01T00:00:00Z"
        );
        assert_eq!(
            Request::PullReviews { number: 7, page: 1 }.path(&r),
            "/repos/EarthOS-world/EarthOS.world/pulls/7/reviews?per_page=100&page=1"
        );
    }
}
