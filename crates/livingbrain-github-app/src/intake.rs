//! `POST /v1/github/webhook` — GitHub's deliveries, verified before they
//! are read.
//!
//! **Which app.** The manifest has to name the webhook URL before GitHub
//! has assigned the app an id, so the URL carries none. GitHub names the
//! app on every delivery instead: `X-GitHub-Hook-Installation-Target-Type:
//! integration` and `X-GitHub-Hook-Installation-Target-ID: <app id>` ("the
//! resource where the webhook was created"). That header is only a
//! *selector* — it picks which stored secret the body must verify under —
//! so a forged one gains nothing: the body still has to carry a valid
//! HMAC under that app's secret. `/webhook/{app_id}` is accepted too (the
//! path is the selector then, and a header that disagrees is refused).
//!
//! **Order.** Size cap → app secret → `livingbrain_github::webhook::verify`
//! over the exact bytes → `ping` answered → `Delivery::parse` →
//! `record_delivery` (a replay is a 200 no-op) → apply. Nothing is read
//! from a body before it verifies, and nothing from GitHub's content is
//! stored: content events leave a **reference** in the queue (`item` /
//! `<number>`, `push`), and the cron drain re-reads current state.
//!
//! **Answers.** 401 for anything that does not verify (no app, no secret,
//! bad or missing signature) — one answer, so a probe learns nothing about
//! which app ids exist. 413 for an oversized body. A verified delivery is
//! a fast 200 whatever it carried, including events this connection does
//! not act on.

use cratefield_core::axum::Json;
use cratefield_core::axum::body::Bytes;
use cratefield_core::axum::extract::{Path, State};
use cratefield_core::axum::http::{HeaderMap, StatusCode};
use cratefield_core::axum::response::{IntoResponse, Response};
use cratefield_core::{Database, Problem, ProblemDef};
use livingbrain_github::webhook::{self, Delivery, Event, WebhookError};
use serde::Deserialize;
use serde_json::json;

use crate::routes::{RouteState, St, port};
use crate::{Account, InstallationStatus, RepoRef, WorkItem};

pub(crate) const UNAUTHORIZED: ProblemDef = ProblemDef {
    slug: "github/webhook-unverified",
    status: StatusCode::UNAUTHORIZED,
    title: "Webhook signature did not verify",
    description: "The delivery is not signed with a secret of an app this deployment holds.",
};

const TARGET_ID: &str = "x-github-hook-installation-target-id";
const TARGET_TYPE: &str = "x-github-hook-installation-target-type";

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn unverified() -> Response {
    Problem::new(&UNAUTHORIZED).into_response()
}

fn accepted(what: &str) -> Response {
    (StatusCode::OK, Json(json!({"ok": true, "handled": what}))).into_response()
}

/// `POST /v1/github/webhook` — the app named by the target header.
pub(crate) async fn webhook(State(state): St, headers: HeaderMap, body: Bytes) -> Response {
    intake(&state, &headers, &body, None).await
}

/// `POST /v1/github/webhook/{app_id}` — the app named by the path.
pub(crate) async fn webhook_for_app(
    State(state): St,
    Path(app_id): Path<i64>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    intake(&state, &headers, &body, Some(app_id)).await
}

async fn intake(
    state: &RouteState,
    headers: &HeaderMap,
    body: &[u8],
    path_app: Option<i64>,
) -> Response {
    if body.len() > crate::MAX_WEBHOOK_BYTES {
        return Problem::request_too_large().into_response();
    }
    if header(headers, TARGET_TYPE).is_some_and(|kind| kind != "integration") {
        return unverified();
    }
    let Some(app_id) = select_app(path_app, header(headers, TARGET_ID)).filter(|id| *id > 0) else {
        return unverified();
    };
    let (Some(kms), Ok(db), Ok(clock)) = (
        state.shared.kms.clone(),
        port(state.ctx.ports.db.clone()),
        port(state.ctx.ports.clock.clone()),
    ) else {
        return unverified();
    };
    let Ok(Some((workspace_id, secret))) = crate::webhook_secret(&*kms, &*db, app_id).await else {
        return unverified();
    };
    let signature = header(headers, webhook::SIGNATURE_HEADER).unwrap_or_default();
    if webhook::verify(secret.expose().as_bytes(), body, signature).is_err() {
        return unverified();
    }
    drop(secret);

    let event = header(headers, webhook::EVENT_HEADER);
    if event == Some("ping") {
        return accepted("ping");
    }
    let delivery = match Delivery::parse(event, header(headers, webhook::DELIVERY_HEADER), body) {
        Ok(delivery) => delivery,
        // Signed by GitHub, but not an event this connection acts on (a
        // permission change can add one): accepted, ignored.
        Err(WebhookError::UnsupportedEvent(_)) => return accepted("ignored"),
        Err(_) => return Problem::validation_failed("malformed delivery").into_response(),
    };
    let now = clock.now();
    match crate::record_delivery(&*db, now, app_id, &delivery.id, delivery.event.as_str()).await {
        Ok(true) => {}
        Ok(false) => return accepted("replay"),
        Err(crate::StoreError::Invalid(_)) => {
            return Problem::validation_failed("malformed delivery id").into_response();
        }
        Err(_) => return Problem::internal().into_response(),
    }
    let Ok(envelope) = serde_json::from_slice::<Envelope>(body) else {
        return Problem::validation_failed("malformed delivery").into_response();
    };
    match apply(&*db, now, app_id, &workspace_id, delivery.event, &envelope).await {
        Ok(what) => accepted(what),
        Err(_) => Problem::internal().into_response(),
    }
}

// ---------------------------------------------------------------------------
// The few ids a delivery is routed by. Part A's payload models carry no
// numeric repository id (the store's key), so the routing fields are read
// here; nothing else in the body is kept.

#[derive(Deserialize)]
struct Envelope {
    action: Option<String>,
    installation: Option<InstallationRef>,
    repository: Option<RepoId>,
    #[serde(default)]
    repositories: Vec<RepoId>,
    #[serde(default)]
    repositories_added: Vec<RepoId>,
    #[serde(default)]
    repositories_removed: Vec<RepoId>,
    issue: Option<Numbered>,
    pull_request: Option<Numbered>,
    #[serde(default)]
    deleted: bool,
}

#[derive(Deserialize)]
struct InstallationRef {
    id: i64,
    account: Option<AccountRef>,
}

#[derive(Deserialize)]
struct AccountRef {
    login: String,
    #[serde(rename = "type", default)]
    kind: String,
}

#[derive(Deserialize)]
struct RepoId {
    id: i64,
    full_name: String,
}

#[derive(Deserialize)]
struct Numbered {
    number: u64,
}

fn refs(repos: &[RepoId]) -> Vec<RepoRef> {
    repos
        .iter()
        .filter(|repo| repo.id > 0)
        .map(|repo| RepoRef {
            repo_id: repo.id,
            full_name: repo.full_name.clone(),
        })
        .collect()
}

/// Applies a verified, first-seen delivery. Answers what it did, for the
/// response body (no content, just a word).
async fn apply(
    db: &dyn Database,
    now: time::OffsetDateTime,
    app_id: i64,
    app_workspace: &str,
    event: Event,
    envelope: &Envelope,
) -> Result<&'static str, crate::StoreError> {
    let Some(installation) = envelope.installation.as_ref().filter(|i| i.id > 0) else {
        return Ok("no-installation");
    };
    let action = envelope.action.as_deref().unwrap_or_default();

    if event == Event::Installation {
        if let Some(account) = &installation.account {
            let account = Account {
                login: account.login.clone(),
                kind: account.kind.clone(),
            };
            // Seen-or-created, never bound: binding is the install callback's.
            crate::observe_installation(db, now, app_id, installation.id, &account).await?;
        }
        let status = match action {
            "deleted" => Some(InstallationStatus::Deleted),
            "suspend" => Some(InstallationStatus::Suspended),
            "unsuspend" => Some(InstallationStatus::Active),
            _ => None,
        };
        if let Some(status) = status {
            crate::set_installation_status(db, now, app_id, installation.id, status).await?;
            return Ok("installation-status");
        }
    }

    let Some(route) = crate::route_installation(db, app_id, installation.id).await? else {
        return Ok("unbound");
    };
    // Belt and braces: an app's installations only ever bind to the app's
    // own workspace (bind_installation), so this never differs.
    if route.workspace_id != app_workspace || route.status != InstallationStatus::Active {
        return Ok("inactive");
    }
    let ws = route.workspace_id.as_str();

    match event {
        Event::Installation => {
            if action == "created" {
                crate::upsert_repos(db, now, ws, installation.id, &refs(&envelope.repositories))
                    .await?;
            }
            Ok("installation")
        }
        Event::InstallationRepositories => {
            crate::upsert_repos(
                db,
                now,
                ws,
                installation.id,
                &refs(&envelope.repositories_added),
            )
            .await?;
            for repo in &envelope.repositories_removed {
                crate::remove_repo(db, now, ws, installation.id, repo.id).await?;
            }
            Ok("repositories")
        }
        Event::Push
        | Event::Issues
        | Event::IssueComment
        | Event::PullRequest
        | Event::PullRequestReview
        | Event::PullRequestReviewComment
        | Event::Release => {
            let Some(repo) = envelope.repository.as_ref().filter(|r| r.id > 0) else {
                return Ok("no-repository");
            };
            crate::note_webhook(db, now, ws, installation.id, repo.id).await?;
            let item = match event {
                // A deleted branch carries nothing new to read.
                Event::Push if envelope.deleted => None,
                Event::Push => Some(WorkItem {
                    repo_id: Some(repo.id),
                    kind: "push".to_owned(),
                    subject: String::new(),
                }),
                Event::Issues | Event::IssueComment => {
                    envelope.issue.as_ref().map(|issue| WorkItem {
                        repo_id: Some(repo.id),
                        kind: "item".to_owned(),
                        subject: issue.number.to_string(),
                    })
                }
                Event::PullRequest | Event::PullRequestReview | Event::PullRequestReviewComment => {
                    envelope.pull_request.as_ref().map(|pr| WorkItem {
                        repo_id: Some(repo.id),
                        kind: "item".to_owned(),
                        subject: pr.number.to_string(),
                    })
                }
                // No release record exists in schema version 1: the delivery
                // is noted on the repository and nothing is queued.
                _ => None,
            };
            match item {
                Some(item) => {
                    crate::enqueue(db, now, ws, installation.id, &item).await?;
                    Ok("queued")
                }
                None => Ok("noted"),
            }
        }
    }
}

/// Which app a delivery claims to be for: the path's id, or the target
/// header's; both present and different is no app at all.
fn select_app(path: Option<i64>, target: Option<&str>) -> Option<i64> {
    let header_app = target.map(|raw| raw.trim().parse::<i64>().ok());
    match (path, header_app) {
        (Some(path), None | Some(Some(_))) if header_app.is_none_or(|h| h == Some(path)) => {
            Some(path)
        }
        (None, Some(Some(id))) => Some(id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::select_app;

    #[test]
    fn the_app_is_selected_by_path_or_header_and_a_disagreement_is_refused() {
        assert_eq!(select_app(None, Some("42")), Some(42));
        assert_eq!(select_app(Some(42), None), Some(42));
        assert_eq!(select_app(Some(42), Some("42")), Some(42));
        assert_eq!(select_app(Some(42), Some("43")), None);
        assert_eq!(select_app(None, Some("x")), None);
        assert_eq!(select_app(Some(42), Some("x")), None);
        assert_eq!(select_app(None, None), None);
    }
}
