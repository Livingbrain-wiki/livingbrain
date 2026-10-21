//! `GET`/`PUT /v1/dreaming`, `GET /v1/dreaming/decisions` and `POST
//! /v1/dreaming/run`: who may read and change a workspace's dreaming, that a
//! model must be one the workspace connected, that a disabled workspace
//! refuses a run, and that the decision log is paged and shows only the
//! facts the caller can read.

use std::sync::Arc;

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, Method, Request, StatusCode, header};
use cratefield_core::axum::{self};
use cratefield_core::{Database, MapConfig, Ports, Signer, Statement, UlidIdGen};
use cratefield_kms::{Dek, Kms, LocalFileKms};
use cratefield_testing::TestHarness;
use livingbrain_access::{Scope, UserId};
use livingbrain_api::{BudgetSettings, CodingAgents, Dreaming, Facts, Wiki};
use livingbrain_compiler::subject_aliases;
use livingbrain_mcp::{Asker, AuthError, BearerAuth, SessionAuth, page_scope};
use livingbrain_pages::{DreamDecisionRow, FactInput, FactStore, Pages};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tower::ServiceExt;

const WORKSPACE: &str = "T0DREAMING";
const OTHER: &str = "T1ELSEWHERE";
const OWNER: &str = "U0OWNER";
const ADMIN: &str = "U0ADMIN";
const PLAIN: &str = "U0PLAIN";
const NOW: i64 = 1_800_000_000;
const PATH: &str = "/v1/dreaming";

/// Bearers are not used here: every call is a browser session.
struct Nobody;

#[async_trait::async_trait]
impl BearerAuth for Nobody {
    async fn authenticate(&self, _ports: &Ports, _token: &str) -> Result<Asker, AuthError> {
        Err(AuthError::Rejected)
    }
}

/// The session cookie, resolved the way the venture resolves it: the tokens
/// module's `session`, as a full member (`token_scopes: None`).
struct Cookie;

#[async_trait::async_trait]
impl SessionAuth for Cookie {
    async fn session(&self, ports: &Ports, headers: &HeaderMap) -> Result<Asker, AuthError> {
        let member = livingbrain_tokens::session(ports, headers)
            .await
            .map_err(|_| AuthError::Rejected)?;
        Ok(Asker {
            workspace_id: member.workspace_id,
            user_id: member.user_id,
            token_scopes: None,
            agent_owner: None,
            role: Some(member.role.as_str().to_owned()),
            credential: None,
        })
    }

    fn same_origin(&self, headers: &HeaderMap) -> Result<(), String> {
        livingbrain_tokens::same_origin(headers).map_err(|refused| refused.detail)
    }
}

struct Kit {
    kit: TestHarness,
    kms: Arc<dyn Kms>,
}

fn kit() -> Kit {
    let kms: Arc<dyn Kms> = Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test", "test")
            .expect("a well-formed key"),
    );
    let wiki = Wiki::new(Arc::clone(&kms), Arc::new(Nobody)).with_session(Arc::new(Cookie));
    let config = Arc::new(MapConfig::from_pairs([(
        "MODEL_KEYS_SECRET",
        "a-test-encryption-secret-of-32-bytes",
    )]));
    let kit = TestHarness::with_ports(
        vec![
            Box::new(livingbrain_workspaces::Workspaces::new()),
            Box::new(livingbrain_tokens::Tokens::new()),
            Box::new(livingbrain_models::Models::new()),
            Box::new(livingbrain_usage::Usage::new()),
            Box::new(livingbrain_accounts::Access::new()),
            Box::new(Pages::new()),
            Box::new(CodingAgents::new(wiki.clone())),
            Box::new(Facts::new(wiki.clone())),
            Box::new(Dreaming::new(wiki)),
            Box::new(BudgetSettings::new()),
        ],
        move |ports| ports.config = config,
    );
    let kit = Kit { kit, kms };
    for workspace in [WORKSPACE, OTHER] {
        kit.exec(&format!(
            "INSERT INTO workspaces (id, name, owner_id, created_at) \
             VALUES ('{workspace}', 'A workspace', '{OWNER}', '2026-01-01T00:00:00Z')"
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

    fn db(&self) -> Arc<dyn Database> {
        self.kit.db.clone()
    }

    fn cookie(&self, workspace: &str, user: &str) -> String {
        use cratefield_core::{Kid, Payload};
        self.kit.signer.sign(&Payload {
            purpose: livingbrain_workspaces::SESSION_PURPOSE.to_owned(),
            subject: json!({"team_id": workspace, "user_id": user, "exp": NOW + 3600}).to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    }

    fn send(&self, method: Method, path: &str, user: &str, body: Option<Value>) -> Res {
        self.send_in(WORKSPACE, method, path, user, body)
    }

    fn send_in(
        &self,
        workspace: &str,
        method: Method,
        path: &str,
        user: &str,
        body: Option<Value>,
    ) -> Res {
        let cookie = format!("__Host-lb_session={}", self.cookie(workspace, user));
        let mut builder = Request::builder()
            .method(method.clone())
            .uri(format!("https://app.brain.test{path}"))
            .header(header::HOST, "app.brain.test")
            .header(header::CONTENT_TYPE, "application/json")
            .header("cookie", cookie);
        if method != Method::GET {
            builder = builder
                .header("sec-fetch-site", "same-origin")
                .header("origin", "https://app.brain.test");
        }
        let request = builder
            .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
            .expect("a request");
        pollster::block_on(async {
            let response = self
                .kit
                .router
                .clone()
                .oneshot(request)
                .await
                .expect("an answer");
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

    fn facts(&self) -> FactStore {
        FactStore::new(
            self.db(),
            Arc::clone(&self.kms),
            Arc::new(self.kit.clock.clone()),
            Arc::new(UlidIdGen),
        )
    }
}

fn slug_is(body: &Value, slug: &str) -> bool {
    body["type"]
        .as_str()
        .is_some_and(|uri| uri.ends_with(&format!("/{slug}")))
}

fn personal(workspace: &str, user: &str) -> String {
    page_scope(workspace, &Scope::User(UserId::new(user.to_owned())))
}

/// Connects `role` in `workspace` to a provider, as the models module stores it.
fn connect(kit: &Kit, workspace: &str, role: &str, model: &str) {
    kit.exec(&format!(
        "INSERT INTO model_connections (workspace_id, role, provider, base_url, model, \
             key_ciphertext, key_last4, fallback_to_managed, status, missing, context_size, \
             checked_at, updated_by, auth, wire) \
         VALUES ('{workspace}', '{role}', 'acme', 'https://models.acme.example/v1', '{model}', \
             X'00', '1234', 0, 'works', '[]', NULL, '2026-01-01T00:00:00Z', '{OWNER}', \
             'bearer', 'openai')"
    ));
}

#[test]
fn a_member_reads_the_settings_and_only_an_owner_or_admin_changes_them() {
    let kit = kit();
    let read = kit.send(Method::GET, PATH, PLAIN, None);
    assert_eq!(read.status, StatusCode::OK, "{}", read.body);
    assert_eq!(read.body["enabled"], json!(true));
    assert_eq!(read.body["rules_enabled"], json!(true));
    assert_eq!(read.body["model"]["role"], json!("main"));
    assert_eq!(read.body["editable"], json!(false));

    let refused = kit.send(Method::PUT, PATH, PLAIN, Some(json!({"enabled": false})));
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(slug_is(&refused.body, "dreaming/forbidden"));
    let refused = kit.send(Method::POST, "/v1/dreaming/run", PLAIN, None);
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);

    let saved = kit.send(
        Method::PUT,
        PATH,
        OWNER,
        Some(json!({"enabled": false, "rules_enabled": false})),
    );
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(saved.body["enabled"], json!(false));
    assert_eq!(saved.body["rules_enabled"], json!(false));
    let again = kit.send(Method::GET, PATH, ADMIN, None);
    assert_eq!(
        again.body["enabled"],
        json!(false),
        "an admin reads the save"
    );
    assert_eq!(again.body["editable"], json!(true));

    // Another workspace's owner is not this workspace's.
    let elsewhere = kit.send_in(OTHER, Method::GET, PATH, OWNER, None);
    assert_eq!(elsewhere.body["enabled"], json!(true));
}

#[test]
fn a_model_is_chosen_only_from_the_ones_the_workspace_connected() {
    let kit = kit();
    let none = kit.send(Method::PUT, PATH, OWNER, Some(json!({"model": "main"})));
    assert_eq!(
        none.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        none.body
    );
    assert!(slug_is(&none.body, "dreaming/model-not-connected"));

    connect(&kit, WORKSPACE, "main", "main-model");
    // Triage is not connected, so it would fall back to Main: not a choice.
    let fallback = kit.send(Method::PUT, PATH, OWNER, Some(json!({"model": "triage"})));
    assert_eq!(
        fallback.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        fallback.body
    );
    let unknown = kit.send(Method::PUT, PATH, OWNER, Some(json!({"model": "editor"})));
    assert_eq!(unknown.status, StatusCode::BAD_REQUEST, "{}", unknown.body);

    connect(&kit, WORKSPACE, "triage", "triage-model");
    let chosen = kit.send(Method::PUT, PATH, OWNER, Some(json!({"model": "triage"})));
    assert_eq!(chosen.status, StatusCode::OK, "{}", chosen.body);
    assert_eq!(chosen.body["model"]["role"], json!("triage"));
    assert_eq!(chosen.body["model"]["model"], json!("triage-model"));
    let models = chosen.body["models"].as_array().expect("the choices");
    let usable: Vec<&Value> = models
        .iter()
        .filter(|m| m["usable"] == json!(true))
        .collect();
    assert_eq!(
        usable.len(),
        2,
        "main and triage are connected, research is not: {models:?}"
    );

    // A save that leaves the model alone keeps the choice.
    let kept = kit.send(
        Method::PUT,
        PATH,
        OWNER,
        Some(json!({"rules_enabled": true})),
    );
    assert_eq!(kept.body["model"]["role"], json!("triage"));
}

#[test]
fn a_run_is_asked_by_an_owner_and_refused_while_dreaming_is_off() {
    let kit = kit();
    let asked = kit.send(Method::POST, "/v1/dreaming/run", OWNER, None);
    assert_eq!(asked.status, StatusCode::ACCEPTED, "{}", asked.body);
    let status = kit.send(Method::GET, PATH, OWNER, None);
    assert_eq!(status.body["status"]["run_requested"], json!(true));

    kit.send(Method::PUT, PATH, OWNER, Some(json!({"enabled": false})));
    let off = kit.send(Method::POST, "/v1/dreaming/run", OWNER, None);
    assert_eq!(off.status, StatusCode::CONFLICT, "{}", off.body);
    assert!(slug_is(&off.body, "dreaming/off"));
}

/// Two passes of decisions in the owner's scope and one in another
/// workspace's, over facts the owner wrote.
fn seed_decisions(kit: &Kit) -> (String, String) {
    let scope = personal(WORKSPACE, OWNER);
    let facts = kit.facts();
    let mut ids = Vec::new();
    for (attribute, value) in [
        ("auth_provider_decision", "Clerk"),
        ("auth_provider_production", "Clerk prod"),
        ("issue_96_status", "blocked"),
    ] {
        let recorded = pollster::block_on(facts.record(
            &scope,
            FactInput {
                subject: "project/demo".to_owned(),
                attribute: attribute.to_owned(),
                value: value.to_owned(),
                valid_from: Some("2026".to_owned()),
                source: Some("source:seed".to_owned()),
                ..FactInput::default()
            },
            "user-writer",
        ))
        .expect("a fact");
        ids.push(recorded.fact.id);
    }
    let log = |scope: &str,
               at: &str,
               seq: usize,
               op: &str,
               fact: &str,
               by: Option<&str>,
               applied: bool,
               reason: Option<&str>| {
        pollster::block_on(facts.dream_log(&DreamDecisionRow {
            scope,
            dream_at: at,
            seq,
            subject: "project/demo",
            op,
            fact_id: fact,
            by_fact_id: by,
            new_valid_to: None,
            new_attribute: None,
            reason,
            applied,
            input_ids: "[]",
            model: "main",
            decided_at: at,
        }))
        .expect("a logged decision");
    };
    log(
        &scope,
        "2027-01-14T10:00:00Z",
        0,
        "supersede",
        &ids[0],
        Some(&ids[1]),
        true,
        None,
    );
    log(
        &scope,
        "2027-01-14T10:00:00Z",
        1,
        "retract",
        &ids[2],
        None,
        true,
        Some("not-state"),
    );
    log(
        &scope,
        "2027-01-13T09:00:00Z",
        0,
        "end",
        &ids[1],
        None,
        false,
        Some("not-older"),
    );
    // Another workspace's owner decided the same kind of thing in theirs.
    let elsewhere = personal(OTHER, OWNER);
    log(
        &elsewhere,
        "2027-01-14T11:00:00Z",
        0,
        "retract",
        &ids[2],
        None,
        true,
        Some("not-state"),
    );
    (scope, elsewhere)
}

#[test]
fn the_decision_log_is_newest_first_paged_and_names_only_readable_facts() {
    let kit = kit();
    seed_decisions(&kit);

    let first = kit.send(Method::GET, "/v1/dreaming/decisions?limit=2", OWNER, None);
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    let items = first.body["decisions"].as_array().expect("decisions");
    assert_eq!(items.len(), 2, "{}", first.body);
    // Newest pass first; within a pass, the order the model gave them.
    assert_eq!(items[0]["op"], json!("supersede"));
    assert_eq!(
        items[0]["fact"]["attribute"],
        json!("auth_provider_decision")
    );
    assert_eq!(items[0]["fact"]["value"], json!("Clerk"));
    assert_eq!(
        items[0]["by"]["attribute"],
        json!("auth_provider_production")
    );
    assert_eq!(items[0]["by"]["value"], json!("Clerk prod"));
    assert_eq!(items[0]["applied"], json!(true));
    assert_eq!(items[0]["model"], json!("main"));
    assert_eq!(items[0]["where"], json!("personal"));
    assert_eq!(items[1]["op"], json!("retract"));
    assert_eq!(items[1]["reason"], json!("not-state"));
    assert_eq!(items[1]["fact"]["attribute"], json!("issue_96_status"));
    assert_eq!(items[1]["fact"]["value"], json!("blocked"));
    let next = first.body["next"].as_str().expect("a next page").to_owned();

    let second = kit.send(
        Method::GET,
        &format!("/v1/dreaming/decisions?limit=2&before={next}"),
        OWNER,
        None,
    );
    let rest = second.body["decisions"].as_array().expect("decisions");
    assert_eq!(rest.len(), 1, "{}", second.body);
    assert_eq!(rest[0]["op"], json!("end"));
    assert_eq!(rest[0]["applied"], json!(false));
    assert_eq!(rest[0]["reason"], json!("not-older"));
    assert!(second.body["next"].is_null(), "the last page has no next");

    // The other workspace's decision never shows in this one.
    let all = kit.send(Method::GET, "/v1/dreaming/decisions?limit=100", OWNER, None);
    assert_eq!(all.body["decisions"].as_array().map(Vec::len), Some(3));

    // A member who is not the owner reads none of the owner's personal decisions.
    let plain = kit.send(Method::GET, "/v1/dreaming/decisions", PLAIN, None);
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
    assert_eq!(plain.body["decisions"], json!([]));

    let bad = kit.send(
        Method::GET,
        "/v1/dreaming/decisions?before=nonsense",
        OWNER,
        None,
    );
    assert_eq!(bad.status, StatusCode::BAD_REQUEST, "{}", bad.body);
}

#[test]
fn the_status_strip_counts_this_workspaces_changes() {
    let kit = kit();
    seed_decisions(&kit);
    // The workspace's scopes are the ones its compile jobs are in: a synced
    // session of the owner's is what puts their personal scope in it.
    kit.exec(&format!(
        "INSERT INTO coding_agent_jobs (scope, body_sha256, source_id, workspace, owner, \
             session_key, source_created_at, status, queued_at, updated_at) \
         VALUES ('{}', 'sha-1', 'source-1', '{WORKSPACE}', '{OWNER}', 'session-1', \
             '2027-01-01T00:00:00Z', 'done', '2027-01-01T00:00:00Z', '2027-01-01T00:00:00Z')",
        personal(WORKSPACE, OWNER)
    ));
    let read = kit.send(Method::GET, PATH, OWNER, None);
    let changes = &read.body["status"]["changes_week"];
    assert_eq!(changes["retract"], json!(1), "{}", read.body);
    assert_eq!(changes["supersede"], json!(1), "{}", read.body);
    assert!(
        changes.get("end").is_none(),
        "a refused end is not a change"
    );
}

/// `PUT /v1/facts/aliases` with `alias` and `canonical`.
fn alias_put(alias: &str, canonical: &str) -> Option<Value> {
    Some(json!({"alias": alias, "canonical": canonical}))
}

#[test]
fn alias_routes_are_read_by_members_and_changed_only_by_an_owner() {
    let kit = kit();
    let plain = kit.send(Method::GET, "/v1/facts/aliases", PLAIN, None);
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
    assert_eq!(plain.body["editable"], json!(false));

    let refused = kit.send(
        Method::PUT,
        "/v1/facts/aliases",
        PLAIN,
        alias_put("project/world369", "project/earthos"),
    );
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.body);
    assert!(slug_is(&refused.body, "facts/aliases/forbidden"));

    let set = kit.send(
        Method::PUT,
        "/v1/facts/aliases",
        OWNER,
        alias_put("project/world369", "project/earthos"),
    );
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);
    let listed = kit.send(Method::GET, "/v1/facts/aliases", OWNER, None);
    let aliases = listed.body["aliases"].as_array().expect("a list");
    assert_eq!(aliases.len(), 1, "{}", listed.body);
    assert_eq!(aliases[0]["state"], json!("set"));
    assert_eq!(aliases[0]["set_by"], json!(OWNER));

    let gone = kit.send(
        Method::DELETE,
        "/v1/facts/aliases?alias=project/world369",
        PLAIN,
        None,
    );
    assert_eq!(gone.status, StatusCode::FORBIDDEN, "{}", gone.body);
    let removed = kit.send(
        Method::DELETE,
        "/v1/facts/aliases?alias=project/world369",
        OWNER,
        None,
    );
    assert_eq!(removed.status, StatusCode::OK, "{}", removed.body);
    let again = kit.send(
        Method::DELETE,
        "/v1/facts/aliases?alias=project/world369",
        OWNER,
        None,
    );
    assert_eq!(again.status, StatusCode::NOT_FOUND, "{}", again.body);
    assert!(slug_is(&again.body, "facts/aliases/not-found"));
}

#[test]
fn an_alias_must_be_two_entities_with_no_date_folder_and_no_chain() {
    let kit = kit();
    let put = |body: Option<Value>| kit.send(Method::PUT, "/v1/facts/aliases", OWNER, body);

    let date = put(alias_put("project/2026-08-14", "project/earthos"));
    assert_eq!(
        date.status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "{}",
        date.body
    );
    assert!(slug_is(&date.body, "facts/aliases/refused"));
    assert!(
        date.body.to_string().contains("bad-subject"),
        "{}",
        date.body
    );

    let same = put(alias_put("project/x", "project/x"));
    assert!(
        same.body.to_string().contains("same-subject"),
        "{}",
        same.body
    );
    let kind = put(alias_put("widget/x", "project/earthos"));
    assert!(
        kind.body.to_string().contains("bad-subject"),
        "{}",
        kind.body
    );
    let partial = put(Some(json!({"alias": "project/a"})));
    assert!(partial.status.is_client_error(), "{}", partial.body);

    let first = put(alias_put("project/world369", "project/earthos"));
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    // `earthos` is now a canonical name: it cannot also be an alias.
    let chain = put(alias_put("project/earthos", "project/other"));
    assert!(
        chain.body.to_string().contains("alias-is-canonical"),
        "{}",
        chain.body
    );
    // And an alias in force cannot be the name others are filed under.
    let to_alias = put(alias_put("project/aura", "project/world369"));
    assert!(
        to_alias.body.to_string().contains("canonical-is-alias"),
        "{}",
        to_alias.body
    );
}

#[test]
fn another_workspace_neither_reads_nor_changes_these_aliases() {
    let kit = kit();
    let set = kit.send(
        Method::PUT,
        "/v1/facts/aliases",
        OWNER,
        alias_put("project/world369", "project/earthos"),
    );
    assert_eq!(set.status, StatusCode::OK, "{}", set.body);

    let other = kit.send_in(OTHER, Method::GET, "/v1/facts/aliases", OWNER, None);
    assert_eq!(other.status, StatusCode::OK, "{}", other.body);
    assert_eq!(other.body["aliases"], json!([]), "{}", other.body);

    let dismiss = kit.send_in(
        OTHER,
        Method::DELETE,
        "/v1/facts/aliases?alias=project/world369",
        OWNER,
        None,
    );
    assert_eq!(dismiss.status, StatusCode::NOT_FOUND, "{}", dismiss.body);

    // The other workspace may map the same name its own way; ours is unchanged.
    let theirs = kit.send_in(
        OTHER,
        Method::PUT,
        "/v1/facts/aliases",
        OWNER,
        alias_put("project/world369", "project/elsewhere"),
    );
    assert_eq!(theirs.status, StatusCode::OK, "{}", theirs.body);
    let ours = kit.send(Method::GET, "/v1/facts/aliases", OWNER, None);
    assert_eq!(
        ours.body["aliases"][0]["canonical"],
        json!("project/earthos")
    );
}

#[test]
fn a_proposal_is_shown_and_applies_only_when_an_owner_accepts_it() {
    let kit = kit();
    let now = OffsetDateTime::from_unix_timestamp(NOW).expect("a time");
    let db = kit.db();
    pollster::block_on(subject_aliases::propose(
        &*db,
        WORKSPACE,
        "project/world369",
        "project/earthos",
        3,
        now,
    ))
    .expect("proposed");
    pollster::block_on(subject_aliases::propose(
        &*db,
        WORKSPACE,
        "project/aura",
        "project/earthos",
        4,
        now,
    ))
    .expect("proposed");

    let view = kit.send(Method::GET, PATH, PLAIN, None);
    assert_eq!(view.status, StatusCode::OK, "{}", view.body);
    assert_eq!(
        view.body["alias_suggestions"].as_array().map(Vec::len),
        Some(2),
        "{}",
        view.body
    );
    assert!(
        pollster::block_on(subject_aliases::load(&*db, WORKSPACE))
            .expect("load")
            .is_empty(),
        "nothing applied"
    );

    // A member cannot accept; an owner can, and it is then in force.
    let member = kit.send(
        Method::PUT,
        "/v1/facts/aliases",
        PLAIN,
        alias_put("project/world369", "project/earthos"),
    );
    assert_eq!(member.status, StatusCode::FORBIDDEN, "{}", member.body);
    let accepted = kit.send(
        Method::PUT,
        "/v1/facts/aliases",
        OWNER,
        alias_put("project/world369", "project/earthos"),
    );
    assert_eq!(accepted.status, StatusCode::OK, "{}", accepted.body);
    let listed = kit.send(Method::GET, "/v1/facts/aliases", OWNER, None);
    let states: Vec<(String, String)> = listed.body["aliases"]
        .as_array()
        .expect("a list")
        .iter()
        .map(|row| (row["alias"].to_string(), row["state"].to_string()))
        .collect();
    assert!(
        states.contains(&("\"project/world369\"".to_owned(), "\"set\"".to_owned())),
        "{states:?}"
    );
    assert!(
        states.contains(&("\"project/aura\"".to_owned(), "\"proposed\"".to_owned())),
        "{states:?}"
    );

    // Dismissing the other proposal takes it off the page for good.
    let dismissed = kit.send(
        Method::DELETE,
        "/v1/facts/aliases?alias=project/aura",
        OWNER,
        None,
    );
    assert_eq!(dismissed.status, StatusCode::OK, "{}", dismissed.body);
    let after = kit.send(Method::GET, PATH, OWNER, None);
    assert_eq!(after.body["alias_suggestions"], json!([]), "{}", after.body);
}

#[test]
fn the_alias_table_is_declared_erasable_by_the_user_who_set_it() {
    use cratefield_core::{Disposition, Module};
    let kms: Arc<dyn Kms> = Arc::new(
        LocalFileKms::from_key(Dek::generate().expect("a key"), "test", "test")
            .expect("a well-formed key"),
    );
    let module = CodingAgents::new(Wiki::new(kms, Arc::new(Nobody)));
    let declared = module
        .personal_data()
        .iter()
        .find(|set| set.table == "fact_subject_aliases")
        .expect("the alias table is declared");
    assert_eq!(
        declared.subject, "set_by",
        "erasure matches the user who set it"
    );
    assert!(matches!(declared.disposition, Disposition::Erase));
}
