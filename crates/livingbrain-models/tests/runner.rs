//! The local runner (`livingbrain_models::jobs`): a member's own runner row,
//! set over `/v1/models/mine`, and the job lifecycle a runner drives — the
//! same call finding its own answer, leases that lapse, and nothing of one
//! member's reachable by another.

mod support;

use std::time::Duration as StdDuration;

use cratefield_core::axum::http::StatusCode;
use cratefield_core::{MapConfig, UlidIdGen};
use cratefield_testing::FixedClock;
use livingbrain_models::invoke::{ChatRequest, InvokeError};
use livingbrain_models::jobs::{self, Dispatch, JobCall, JobError};
use serde_json::json;
use support::{FakeHttp, OWNER, PLAIN, WORKSPACE, delete, get, put};
use time::{Duration, OffsetDateTime};

const SECRET: &str = "a-test-encryption-secret-of-32-bytes";

#[test]
fn any_member_sets_their_own_runner_and_only_their_own() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, owner) = support::signed_in(&http).await;
        let plain = support::session(&kit, WORKSPACE, PLAIN);

        // A plain member — not an admin — may: it spends their own
        // subscription, never the workspace's key.
        let set = put(
            &kit,
            "/v1/models/mine/main",
            &plain,
            json!({"kind": "local_runner", "agent": "codex", "model": "gpt-5.5"}),
        )
        .await;
        assert_eq!(set.status, StatusCode::OK, "{}", set.text());
        assert_eq!(set.json()["agent"], "codex");
        assert_eq!(set.json()["settings"], "isolated");

        // The owner sees none of it; the plain member sees theirs.
        let theirs = get(&kit, "/v1/models/mine", &owner).await.json();
        assert_eq!(theirs["runners"].as_array().map(Vec::len), Some(0));
        let mine = get(&kit, "/v1/models/mine", &plain).await.json();
        assert_eq!(mine["runners"][0]["model"], "gpt-5.5");
        assert!(mine["runners"][0]["last_seen_at"].is_null());

        // The workspace's connections are untouched by a runner.
        let listed = get(&kit, "/v1/models", &owner).await.json();
        assert_eq!(listed.as_array().map(Vec::len), Some(0));

        for (body, why) in [
            (json!({"kind": "api_key", "agent": "claude"}), "kind"),
            (json!({"kind": "local_runner", "agent": "gemini"}), "agent"),
            (
                json!({"kind": "local_runner", "agent": "claude", "model": "a b"}),
                "model",
            ),
            (
                json!({"kind": "local_runner", "agent": "claude", "settings": "all"}),
                "settings",
            ),
        ] {
            let refused = put(&kit, "/v1/models/mine/main", &plain, body).await;
            assert_eq!(
                refused.status,
                StatusCode::BAD_REQUEST,
                "{why}: {}",
                refused.text()
            );
        }
        let refused = put(
            &kit,
            "/v1/models/mine/editor",
            &plain,
            json!({"kind": "local_runner", "agent": "claude"}),
        )
        .await;
        assert_eq!(refused.status, StatusCode::BAD_REQUEST);

        let gone = delete(&kit, "/v1/models/mine/main", &plain).await;
        assert_eq!(gone.status, StatusCode::NO_CONTENT);
        let mine = get(&kit, "/v1/models/mine", &plain).await.json();
        assert_eq!(mine["runners"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            get(&kit, "/v1/models/mine", "").await.status,
            StatusCode::UNAUTHORIZED
        );
    });
}

fn request<'a>(user: &'a str) -> ChatRequest<'a> {
    ChatRequest {
        system: "rules",
        user,
        max_tokens: 100,
        json: true,
        structured: None,
        timeout: StdDuration::from_secs(5),
    }
}

#[test]
fn a_job_is_claimed_by_its_owner_answered_and_found_again() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = support::signed_in(&http).await;
        let db = kit.db.clone();
        let config = MapConfig::from_pairs([("MODEL_KEYS_SECRET", SECRET)]);
        let at = OffsetDateTime::from_unix_timestamp(support::NOW).expect("an instant");
        let clock = FixedClock(at);
        let ids = UlidIdGen;
        let ports = jobs::Ports {
            db: &*db,
            http: &http,
            config: &config,
            clock: &clock,
            id_gen: &ids,
        };
        let call = JobCall {
            owner: PLAIN,
            kind: "compile",
            ref_key: "scope:sha:0",
        };

        // No runner and no connection: no model, and nothing queued.
        let none = jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u")).await;
        assert_eq!(none, Err(InvokeError::NoModel));

        jobs::set_runner(
            &*db, at, WORKSPACE, PLAIN, "main", "claude", "haiku", "isolated",
        )
        .await
        .expect("a runner");
        assert!(
            jobs::available(&*db, WORKSPACE, PLAIN, "triage")
                .await
                .expect("read")
        );
        assert!(
            !jobs::available(&*db, WORKSPACE, OWNER, "main")
                .await
                .expect("read")
        );
        let Ok(Dispatch::Pending { job_id }) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u")).await
        else {
            panic!("the call goes to the runner");
        };
        // The same call again is the same job, still waiting.
        assert_eq!(
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u")).await,
            Ok(Dispatch::Pending {
                job_id: job_id.clone()
            })
        );
        // The prompt is sealed at rest.
        let raw = db
            .query(&cratefield_core::Statement::new(
                "SELECT payload FROM model_jobs",
            ))
            .await
            .expect("a read");
        let blob = raw
            .first()
            .and_then(|r| r.get::<Vec<u8>>("payload"))
            .expect("sealed");
        assert!(!String::from_utf8_lossy(&blob).contains("rules"));

        // Only the owner's runner finds it.
        assert_eq!(
            jobs::claim_next(&*db, &config, at, WORKSPACE, OWNER).await,
            Ok(None)
        );
        assert_eq!(
            jobs::claim_next(&*db, &config, at, "T0OTHER", PLAIN).await,
            Ok(None)
        );
        let claimed = jobs::claim_next(&*db, &config, at, WORKSPACE, PLAIN)
            .await
            .expect("a claim")
            .expect("the job");
        assert_eq!(claimed.id, job_id);
        assert_eq!(
            (claimed.system.as_str(), claimed.user.as_str()),
            ("rules", "u")
        );
        assert_eq!(
            (claimed.agent.as_str(), claimed.model.as_str()),
            ("claude", "haiku")
        );
        // Leased: nothing more to take until the lease lapses.
        assert_eq!(
            jobs::claim_next(&*db, &config, at, WORKSPACE, PLAIN).await,
            Ok(None)
        );
        // Nobody else can answer or fail it, or read it.
        assert_eq!(
            jobs::complete(
                &*db, &config, &ids, at, WORKSPACE, OWNER, &job_id, "x", 1, 1
            )
            .await,
            Err(JobError::NotFound)
        );
        assert_eq!(
            jobs::fail(&*db, at, WORKSPACE, OWNER, &job_id, "timeout").await,
            Err(JobError::NotFound)
        );
        assert_eq!(
            jobs::get(&*db, WORKSPACE, OWNER, &job_id)
                .await
                .expect("read"),
            None
        );

        let done = jobs::complete(
            &*db, &config, &ids, at, WORKSPACE, PLAIN, &job_id, "{}", 7, 3,
        )
        .await
        .expect("stored");
        assert_eq!(done.status, "done");
        let Ok(Dispatch::Reply(reply)) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u")).await
        else {
            panic!("the answer is handed back");
        };
        assert_eq!(reply.text, "{}");
        assert!(reply.runner);
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (7, 3));
        assert_eq!(reply.provider, "local-runner:claude");
        let row = jobs::get(&*db, WORKSPACE, PLAIN, &job_id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.status, "consumed");
        // Consumed: asked once more, it is a new job.
        let Ok(Dispatch::Pending { job_id: again }) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u")).await
        else {
            panic!("a new job");
        };
        assert_ne!(again, job_id);
        // A changed prompt under the same handle expires the waiting one.
        let Ok(Dispatch::Pending { job_id: changed }) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("u2")).await
        else {
            panic!("a new job");
        };
        assert_ne!(changed, again);
        let old = jobs::get(&*db, WORKSPACE, PLAIN, &again)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(old.status, "expired");
    });
}

#[test]
fn a_lapsed_lease_is_reclaimed_until_the_attempts_run_out() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = support::signed_in(&http).await;
        let db = kit.db.clone();
        let config = MapConfig::from_pairs([("MODEL_KEYS_SECRET", SECRET)]);
        let at = OffsetDateTime::from_unix_timestamp(support::NOW).expect("an instant");
        let clock = FixedClock(at);
        let ids = UlidIdGen;
        let ports = jobs::Ports {
            db: &*db,
            http: &http,
            config: &config,
            clock: &clock,
            id_gen: &ids,
        };
        jobs::set_runner(&*db, at, WORKSPACE, PLAIN, "main", "codex", "", "isolated")
            .await
            .expect("a runner");
        let call = JobCall {
            owner: PLAIN,
            kind: "ask",
            ref_key: "q",
        };
        let Ok(Dispatch::Pending { job_id }) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("why?")).await
        else {
            panic!("pending");
        };
        let mut now = at;
        for attempt in 1..=3 {
            let claimed = jobs::claim_next(&*db, &config, now, WORKSPACE, PLAIN)
                .await
                .expect("a claim");
            assert_eq!(
                claimed.map(|c| c.id),
                Some(job_id.clone()),
                "attempt {attempt}"
            );
            now += jobs::LEASE + Duration::seconds(1);
        }
        // Three lapsed leases: the job is failed, not claimed a fourth time.
        assert_eq!(
            jobs::claim_next(&*db, &config, now, WORKSPACE, PLAIN).await,
            Ok(None)
        );
        let row = jobs::get(&*db, WORKSPACE, PLAIN, &job_id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(
            (row.status.as_str(), row.error.as_deref()),
            ("failed", Some("timeout"))
        );
        // The caller hears the failure once.
        assert_eq!(
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("why?")).await,
            Err(InvokeError::Runner("timeout".to_owned()))
        );
        // A job nobody claims expires after its TTL, and its prompt is wiped.
        let Ok(Dispatch::Pending { job_id: late }) =
            jobs::dispatch(&ports, WORKSPACE, "main", &call, &request("why?")).await
        else {
            panic!("pending");
        };
        let later = at + jobs::TTL + Duration::seconds(1);
        assert_eq!(
            jobs::claim_next(&*db, &config, later, WORKSPACE, PLAIN).await,
            Ok(None)
        );
        let row = jobs::get(&*db, WORKSPACE, PLAIN, &late)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.status, "expired");
        // An unknown failure reason is stored as `cli-error`, never verbatim.
        let reasons = jobs::FAIL_REASONS;
        assert!(reasons.contains(&"timeout") && !reasons.contains(&"anything"));
    });
}
