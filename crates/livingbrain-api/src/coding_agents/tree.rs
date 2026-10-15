//! `GET /v1/coding-agents/tree` — the asker's synced sessions as the wiki's
//! `coding-agents/` folders: agent, then project, each with how many
//! sessions it holds, how many of them compiled, and where the rest stand
//! in the queue. It is what the home view draws before (and beside) the
//! pages the compiler writes, so a person sees `colonizer/` the moment
//! their first colony is synced, not once a model has compiled it.
//!
//! Read from the compile queue, which holds one job per `agent_log` body
//! (the read enqueues whatever the ledger has that has none yet, as the
//! status route does), under the same two clauses: the asker's workspace
//! and the asker's scopes. Only columns: no body, no page.

use std::collections::BTreeMap;
use std::sync::Arc;

use cratefield_core::Problem;
use cratefield_core::axum::Json;
use cratefield_core::axum::extract::State;
use cratefield_core::axum::http::{HeaderMap, Method};
use livingbrain_compiler::layout::agent_segment;
use livingbrain_compiler::{Job, JobStatus, Queue};
use serde_json::{Value, json};

use crate::port;
use crate::service::Service;

/// The most jobs one read groups. A workspace past it is answered from its
/// newest jobs and says so (`truncated`).
const MAX_JOBS: usize = 5000;

/// The agent a session key names: `<agent> session <id>`, the title every
/// producer files a session under.
pub(super) fn agent_of(session_key: &str) -> Option<&str> {
    let (agent, _) = session_key.split_once(" session ")?;
    let agent = agent.trim();
    (!agent.is_empty()).then_some(agent)
}

/// One project's numbers.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ProjectNode {
    pub(super) sessions: u64,
    pub(super) compiled: u64,
    /// Each session's newest job, by status.
    pub(super) statuses: BTreeMap<&'static str, u64>,
    pub(super) latest_at: String,
}

/// Jobs (newest first) to `agent → project → numbers`. A session is counted
/// once, by its newest body; it is compiled when any of its bodies is.
pub(super) fn group(jobs: &[Job]) -> BTreeMap<String, BTreeMap<String, ProjectNode>> {
    struct Session<'a> {
        newest: &'a Job,
        compiled: bool,
    }
    let mut sessions: BTreeMap<(&str, &str), Session<'_>> = BTreeMap::new();
    for job in jobs {
        let key = (job.scope.as_str(), job.session_key.as_str());
        let done = job.status == JobStatus::Done;
        sessions
            .entry(key)
            .and_modify(|session| {
                session.compiled |= done;
                if job.source_created_at > session.newest.source_created_at {
                    session.newest = job;
                }
            })
            .or_insert(Session {
                newest: job,
                compiled: done,
            });
    }
    let mut tree: BTreeMap<String, BTreeMap<String, ProjectNode>> = BTreeMap::new();
    for session in sessions.values() {
        let job = session.newest;
        let Some(agent) = agent_of(&job.session_key) else {
            continue;
        };
        let node = tree
            .entry(agent.to_owned())
            .or_default()
            .entry(job.project.clone().unwrap_or_default())
            .or_default();
        node.sessions += 1;
        node.compiled += u64::from(session.compiled);
        *node.statuses.entry(job.status.as_str()).or_default() += 1;
        if job.source_created_at > node.latest_at {
            node.latest_at.clone_from(&job.source_created_at);
        }
    }
    tree
}

/// The grouped tree as the route answers it: agents alphabetically, each
/// with its wiki folder name and totals, projects alphabetically.
pub(super) fn view(
    tree: &BTreeMap<String, BTreeMap<String, ProjectNode>>,
    truncated: bool,
) -> Value {
    let agents: Vec<Value> = tree
        .iter()
        .map(|(agent, projects)| {
            let sessions: u64 = projects.values().map(|node| node.sessions).sum();
            let compiled: u64 = projects.values().map(|node| node.compiled).sum();
            json!({
                "agent": agent,
                "folder": agent_segment(agent),
                "sessions": sessions,
                "compiled": compiled,
                "projects": projects
                    .iter()
                    .map(|(project, node)| json!({
                        "project": if project.is_empty() { Value::Null } else { json!(project) },
                        "sessions": node.sessions,
                        "compiled": node.compiled,
                        "statuses": node.statuses,
                        "latest_at": node.latest_at,
                    }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({ "agents": agents, "truncated": truncated })
}

/// `GET /tree` — see the module docs.
pub(super) async fn tree(
    State(service): State<Arc<Service>>,
    headers: HeaderMap,
) -> Result<Json<Value>, Problem> {
    let asker = service.asker(&Method::GET, &headers).await?;
    let scopes = service.scopes(&asker);
    let ctx = service.ctx();
    let db = port(ctx.ports.db.clone())?;
    let clock = port(ctx.ports.clock.clone())?;
    let queue = Queue::new(db);
    queue
        .enqueue_from_ledger(clock.now(), Some(&asker.workspace_id))
        .await
        .map_err(|_| Problem::internal())?;
    let jobs = queue
        .list(&asker.workspace_id, &scopes, None, MAX_JOBS)
        .await
        .map_err(|_| Problem::internal())?;
    Ok(Json(view(&group(&jobs), jobs.len() == MAX_JOBS)))
}

#[cfg(test)]
mod tests {
    use super::{agent_of, group, view};
    use livingbrain_compiler::{Job, JobStatus};

    fn job(key: &str, project: Option<&str>, status: JobStatus, at: &str) -> Job {
        Job {
            scope: "user-a".to_owned(),
            body_sha256: format!("{key}{at}"),
            source_id: format!("src-{at}"),
            workspace: "ws".to_owned(),
            owner: "u".to_owned(),
            session_key: key.to_owned(),
            project: project.map(str::to_owned),
            source_created_at: at.to_owned(),
            status,
            attempts: 0,
            last_error: None,
            prompt_tokens: 0,
            completion_tokens: 0,
            pages: Vec::new(),
            rejected: 0,
            facts: 0,
            queued_at: at.to_owned(),
            updated_at: at.to_owned(),
            lease_until: None,
            next_attempt_at: None,
            chunks_done: 0,
            triage: None,
            triage_reason: None,
            triage_plan: None,
            force_full: false,
            est_tokens_raw: 0,
            est_tokens_sent: 0,
        }
    }

    #[test]
    fn the_agent_is_the_title_before_session() {
        assert_eq!(agent_of("colonizer session 1b67"), Some("colonizer"));
        assert_eq!(agent_of("claude_code session x"), Some("claude_code"));
        assert_eq!(agent_of("notes/one.md"), None);
    }

    /// One row per session (its newest body), compiled when any body did;
    /// agents and projects as folders, the compiler's folder names beside.
    #[test]
    fn sessions_group_by_agent_and_project() {
        let jobs = [
            job(
                "colonizer session a",
                Some("harness"),
                JobStatus::Queued,
                "2026-10-03",
            ),
            job(
                "colonizer session a",
                Some("harness"),
                JobStatus::Done,
                "2026-10-01",
            ),
            job(
                "colonizer session b",
                Some("harness"),
                JobStatus::Queued,
                "2026-10-02",
            ),
            job(
                "claude_code session c",
                Some("demo"),
                JobStatus::Failed,
                "2026-10-02",
            ),
            job("claude_code session d", None, JobStatus::Done, "2026-10-01"),
        ];
        let tree = group(&jobs);
        let harness = &tree["colonizer"]["harness"];
        assert_eq!(harness.sessions, 2);
        assert_eq!(harness.compiled, 1);
        assert_eq!(harness.statuses.get("queued"), Some(&2));
        assert_eq!(harness.latest_at, "2026-10-03");
        let value = view(&tree, false);
        assert_eq!(value["agents"][0]["agent"], "claude_code");
        assert_eq!(value["agents"][0]["folder"], "claude");
        assert_eq!(value["agents"][0]["sessions"], 2);
        assert_eq!(
            value["agents"][0]["projects"][0]["project"],
            serde_json::Value::Null
        );
        assert_eq!(value["agents"][1]["folder"], "colonizer");
        assert_eq!(value["agents"][1]["compiled"], 1);
        assert_eq!(value["truncated"], false);
    }
}
