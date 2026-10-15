//! `GET`/`PATCH /v1/settings/budgets`: the workspace's budgets and limits as
//! settings — who may read and change them, the precedence (setting > env
//! var > default), the ranges, the same-origin rule for a cookie write, one
//! workspace never touching another's, and the compiler's queue card and
//! "Process now" running under the effective values.

use std::sync::Arc;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{Method, Request, StatusCode, header};
use cratefield_core::axum::{self, Router};
use cratefield_core::{MapConfig, Ports, Signer, Statement};
use cratefield_kms::{Dek, LocalFileKms};
use cratefield_testing::TestHarness;
use livingbrain_api::{BudgetSettings, CodingAgents, Wiki};
use livingbrain_mcp::{Asker, AuthError, BearerAuth};
use serde_json::{Value, json};
use tower::ServiceExt;

const WORKSPACE: &str = "T0BUDGETS";
const OTHER: &str = "T1ELSEWHERE";
const OWNER: &str = "U0OWNER";
const ADMIN: &str = "U0ADMIN";
const PLAIN: &str = "U0PLAIN";
const NOW: i64 = 1_800_000_000;
const PATH: &str = "/v1/settings/budgets";

/// The coding-agents routes answer to the venture's bearer seam; these
/// tests reach them by cookie through the tokens module instead, so the
/// seam refuses everything.
struct Nobody;

#[async_trait::async_trait]
impl BearerAuth for Nobody {
    async fn authenticate(&self, _ports: &Ports, _token: &str) -> Result<Asker, AuthError> {
        Err(AuthError::Rejected)
    }
}

struct Kit {
    kit: TestHarness,
}

fn kit(env: &[(&str, &str)]) -> Kit {
    let kms = LocalFileKms::from_key(Dek::generate().expect("a key"), "test", "test")
        .expect("a well-formed key");
    let wiki = Wiki::new(Arc::new(kms), Arc::new(Nobody));
    let config = Arc::new(MapConfig::from_pairs(
        env.iter()
            .copied()
            .chain([("MODEL_KEYS_SECRET", "a-test-encryption-secret-of-32-bytes")]),
    ));
    let kit = TestHarness::with_ports(
        vec![
            Box::new(livingbrain_workspaces::Workspaces::new()),
            Box::new(livingbrain_tokens::Tokens::new()),
            Box::new(livingbrain_models::Models::new()),
            Box::new(livingbrain_usage::Usage::new()),
            // Mounted for its tables: the queue's spend counts the read
            // shows against the compile budgets.
            Box::new(CodingAgents::new(wiki)),
            Box::new(BudgetSettings::new()),
        ],
        move |ports| ports.config = config,
    );
    let kit = Kit { kit };
    for (workspace, owner) in [(WORKSPACE, OWNER), (OTHER, OWNER)] {
        kit.exec(&format!(
            "INSERT INTO workspaces (id, name, owner_id, created_at) \
             VALUES ('{workspace}', 'A workspace', '{owner}', '2026-01-01T00:00:00Z')"
        ));
    }
    for (workspace, user, role) in [
        (WORKSPACE, OWNER, "owner"),
        (WORKSPACE, ADMIN, "admin"),
        (WORKSPACE, PLAIN, "member"),
        (OTHER, OWNER, "owner"),
    ] {
        kit.exec(&format!(
            "INSERT INTO workspace_members \
                 (workspace_id, user_id, name, timezone, is_admin, updated_at, role) \
             VALUES ('{workspace}', '{user}', 'A member', NULL, 0, '2026-01-01T00:00:00Z', \
                     '{role}')"
        ));
    }
    kit
}

struct Res {
    status: StatusCode,
    body: Value,
}

impl Kit {
    fn exec(&self, sql: &str) {
        pollster::block_on(self.kit.db.execute(&Statement::new(sql))).expect("the seed runs");
    }

    fn router(&self) -> Router {
        self.kit.router.clone()
    }

    /// A session cookie for `user` in `workspace`, sealed the way the
    /// workspaces module seals one.
    fn cookie(&self, workspace: &str, user: &str) -> String {
        use cratefield_core::{Kid, Payload};
        self.kit.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({"team_id": workspace, "user_id": user, "exp": NOW + 3600}).to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }

    fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, String)],
        body: Option<Value>,
    ) -> Res {
        let mut builder = Request::builder()
            .method(method)
            .uri(format!("https://app.brain.test{path}"))
            .header(header::HOST, "app.brain.test")
            .header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            builder = builder.header(*name, value.as_str());
        }
        let request = builder
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .expect("a request");
        pollster::block_on(async {
            let response = self.router().oneshot(request).await.expect("an answer");
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("a body");
            Res {
                status,
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            }
        })
    }

    fn get(&self, user: &str) -> Res {
        self.get_in(WORKSPACE, user)
    }

    fn get_in(&self, workspace: &str, user: &str) -> Res {
        let cookie = format!("__Host-lb_session={}", self.cookie(workspace, user));
        self.send(Method::GET, PATH, &[("cookie", cookie)], None)
    }

    /// A same-origin cookie `PATCH`, the way the settings page sends one.
    fn patch(&self, user: &str, body: Value) -> Res {
        let cookie = format!("__Host-lb_session={}", self.cookie(WORKSPACE, user));
        self.send(
            Method::PATCH,
            PATH,
            &[
                ("cookie", cookie),
                ("sec-fetch-site", "same-origin".to_owned()),
                ("origin", "https://app.brain.test".to_owned()),
            ],
            Some(body),
        )
    }
}

/// Whether a problem body's `type` URI ends in `slug`.
fn slug_is(body: &Value, slug: &str) -> bool {
    body["type"]
        .as_str()
        .is_some_and(|uri| uri.ends_with(&format!("/{slug}")))
}

fn setting<'a>(body: &'a Value, group: &str, key: &str) -> &'a Value {
    &body["settings"][group][key]
}

/// A member reads every limit with where it comes from; nothing is set, so
/// each is the deployment's: the env var where one is set, the default
/// otherwise.
#[test]
fn a_member_reads_the_effective_limits_and_their_source() {
    let kit = kit(&[
        ("CODING_AGENTS_DAILY_TOKENS", "3000000"),
        ("CODING_AGENTS_BATCH", "3"),
    ]);
    let res = kit.get(PLAIN);
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let body = &res.body;
    assert_eq!(body["editable"], json!(false), "a member reads only");
    assert_eq!(body["role"], json!("member"));
    let daily = setting(body, "coding_agents", "daily_tokens");
    assert_eq!(daily["value"], json!(3_000_000), "the env var");
    assert_eq!(daily["source"], json!("default"));
    assert_eq!(setting(body, "coding_agents", "batch")["value"], json!(3));
    assert_eq!(
        setting(body, "coding_agents", "monthly_tokens")["value"],
        json!(4_000_000),
        "the built-in default"
    );
    assert_eq!(
        setting(body, "coding_agents", "paused")["value"],
        json!(false)
    );
    assert_eq!(
        setting(body, "ask", "context_chars")["value"],
        json!(200_000)
    );
    assert_eq!(
        setting(body, "ask", "max_output_tokens")["value"],
        json!(1024)
    );
    assert_eq!(setting(body, "spend", "monthly_cents")["value"], json!(300));
    assert_eq!(setting(body, "coding_agents", "batch")["max"], json!(20));
    let concurrency = setting(body, "coding_agents", "concurrency");
    assert_eq!(concurrency["value"], json!(4), "the built-in default");
    assert_eq!(concurrency["max"], json!(8));
    assert_eq!(body["ceilings"]["concurrency"], json!(8));
    assert_eq!(body["usage"]["compile_tokens_today"], json!(0));
    assert_eq!(body["usage"]["compile_tokens_month"], json!(0));
    assert_eq!(body["ceilings"]["job_tokens"], json!(1_000_000));
}

/// An owner or admin sets a limit, which then wins over the env var; `null`
/// clears it back to the env var.
#[test]
fn an_owner_or_admin_sets_and_clears_a_limit() {
    let kit = kit(&[("CODING_AGENTS_DAILY_TOKENS", "3000000")]);
    let res = kit.patch(
        OWNER,
        json!({"coding_agents": {"daily_tokens": 9_000_000, "paused": true},
               "spend": {"monthly_cents": 2_500}}),
    );
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let daily = setting(&res.body, "coding_agents", "daily_tokens");
    assert_eq!(daily["value"], json!(9_000_000));
    assert_eq!(daily["source"], json!("workspace"));
    assert_eq!(daily["inherited"], json!(3_000_000));
    assert_eq!(
        setting(&res.body, "coding_agents", "paused")["value"],
        json!(true)
    );
    assert_eq!(
        setting(&res.body, "spend", "monthly_cents")["value"],
        json!(2_500)
    );
    assert_eq!(res.body["editable"], json!(true));

    // A member sees what the owner chose.
    let read = kit.get(PLAIN);
    assert_eq!(
        setting(&read.body, "coding_agents", "daily_tokens")["value"],
        json!(9_000_000)
    );

    // An admin clears it; the env var is back, the rest unchanged.
    let res = kit.patch(ADMIN, json!({"coding_agents": {"daily_tokens": null}}));
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let daily = setting(&res.body, "coding_agents", "daily_tokens");
    assert_eq!(daily["value"], json!(3_000_000));
    assert_eq!(daily["source"], json!("default"));
    assert_eq!(
        setting(&res.body, "spend", "monthly_cents")["value"],
        json!(2_500)
    );

    // The usage ledger's verdict reads the same spend limit.
    let cookie = format!("__Host-lb_session={}", kit.cookie(WORKSPACE, OWNER));
    let daily = kit.send(Method::GET, "/v1/usage/daily", &[("cookie", cookie)], None);
    assert_eq!(daily.body["budget_cents"], json!(2_500), "{}", daily.body);
}

/// A member may read but not change; nothing is written.
#[test]
fn a_member_cannot_change_a_limit() {
    let kit = kit(&[]);
    let res = kit.patch(PLAIN, json!({"coding_agents": {"batch": 5}}));
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
    assert!(slug_is(&res.body, "settings/forbidden"), "{}", res.body);
    let read = kit.get(OWNER);
    assert_eq!(
        setting(&read.body, "coding_agents", "batch")["source"],
        json!("default")
    );
}

/// No credential is a 401, for the read and the write alike.
#[test]
fn both_routes_need_a_credential() {
    let kit = kit(&[]);
    assert_eq!(
        kit.send(Method::GET, PATH, &[], None).status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        kit.send(Method::PATCH, PATH, &[], Some(json!({}))).status,
        StatusCode::UNAUTHORIZED
    );
}

/// A cookie write from another site is refused before anything is read.
#[test]
fn a_cookie_write_from_another_origin_is_refused() {
    let kit = kit(&[]);
    let cookie = format!("__Host-lb_session={}", kit.cookie(WORKSPACE, OWNER));
    for headers in [
        vec![
            ("cookie", cookie.clone()),
            ("sec-fetch-site", "cross-site".to_owned()),
        ],
        vec![
            ("cookie", cookie.clone()),
            ("origin", "https://evil.example".to_owned()),
        ],
    ] {
        let res = kit.send(
            Method::PATCH,
            PATH,
            &headers,
            Some(json!({"coding_agents": {"paused": true}})),
        );
        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
        assert!(slug_is(&res.body, "api/cross-site-request"), "{}", res.body);
    }
    let read = kit.get(OWNER);
    assert_eq!(
        setting(&read.body, "coding_agents", "paused")["value"],
        json!(false)
    );
}

/// A value out of range, of the wrong kind, or naming no setting is one 422
/// that names the setting; nothing is saved.
#[test]
fn a_bad_value_is_a_422_that_names_the_setting() {
    let kit = kit(&[]);
    for (body, needle) in [
        (
            json!({"coding_agents": {"batch": 21}}),
            "coding_agents.batch",
        ),
        (
            json!({"coding_agents": {"job_tokens": 5_000_000}}),
            "coding_agents.job_tokens",
        ),
        (
            json!({"coding_agents": {"concurrency": 9}}),
            "coding_agents.concurrency",
        ),
        (
            json!({"coding_agents": {"daily_tokens": "lots"}}),
            "whole number",
        ),
        (json!({"ask": {"context_chars": 10}}), "ask.context_chars"),
        (
            json!({"spend": {"monthly_cents": -5}}),
            "spend.monthly_cents",
        ),
        (json!({"coding_agents": {"paused": "yes"}}), "true, false"),
        (json!({"coding_agents": {"turbo": 1}}), "not a setting"),
    ] {
        let res = kit.patch(OWNER, body.clone());
        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            res.body
        );
        let detail = res.body["detail"].as_str().unwrap_or_default();
        assert!(detail.contains(needle), "{body}: {detail}");
    }
    // A batch inside the ceiling is fine, and the ceiling is the edge.
    let res = kit.patch(OWNER, json!({"coding_agents": {"batch": 20}}));
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let res = kit.patch(OWNER, json!({"coding_agents": {"turbo": 1, "batch": 2}}));
    assert_eq!(res.status, StatusCode::UNPROCESSABLE_ENTITY);
    let read = kit.get(OWNER);
    assert_eq!(
        setting(&read.body, "coding_agents", "batch")["value"],
        json!(20),
        "a refused patch saves nothing, not even its valid half"
    );
}

/// One workspace's limits are its own: setting them changes nothing in
/// another, even for the same person.
#[test]
fn a_workspace_never_sees_or_changes_another_s_limits() {
    let kit = kit(&[]);
    let res = kit.patch(OWNER, json!({"coding_agents": {"monthly_tokens": 1}}));
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    let elsewhere = kit.get_in(OTHER, OWNER);
    assert_eq!(elsewhere.status, StatusCode::OK);
    assert_eq!(elsewhere.body["workspace_id"], json!(OTHER));
    let monthly = setting(&elsewhere.body, "coding_agents", "monthly_tokens");
    assert_eq!(monthly["value"], json!(4_000_000));
    assert_eq!(monthly["source"], json!("default"));
}

/// A full-access personal access token changes a limit the way the cookie
/// does, with no browser origin to check; a member's token still cannot.
#[test]
fn a_bearer_token_changes_a_limit_for_an_owner_only() {
    let kit = kit(&[]);
    let mint = |user: &str| {
        let cookie = format!("__Host-lb_session={}", kit.cookie(WORKSPACE, user));
        let created = kit.send(
            Method::POST,
            "/v1/tokens",
            &[("cookie", cookie)],
            Some(json!({"name": "a script"})),
        );
        assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
        created.body["token"]
            .as_str()
            .expect("the token")
            .to_owned()
    };
    let owner = mint(OWNER);
    let res = kit.send(
        Method::PATCH,
        PATH,
        &[("authorization", format!("Bearer {owner}"))],
        Some(json!({"ask": {"max_output_tokens": 2048}})),
    );
    assert_eq!(res.status, StatusCode::OK, "{}", res.body);
    assert_eq!(
        setting(&res.body, "ask", "max_output_tokens")["value"],
        json!(2048)
    );

    let plain = mint(PLAIN);
    let res = kit.send(
        Method::PATCH,
        PATH,
        &[("authorization", format!("Bearer {plain}"))],
        Some(json!({"ask": {"max_output_tokens": 4096}})),
    );
    assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.body);
}
