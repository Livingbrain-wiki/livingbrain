//! `models`: Jev, the compiler's optional triage classifier, per workspace.
//!
//! The precedence (the workspace's switch, then its own key, then the
//! deployment's, else off), the switch that turns Jev off even when the
//! deployment has a key, the workspace key that never comes back in a
//! response or sits in plaintext in its row, and the Test call over a fake
//! classifier.

mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cratefield_core::axum::http::{Method, StatusCode};
use cratefield_core::{
    Answer, Calibration, Classifier, ClassifierError, ClassifierProfile, DEFAULT_MAX_STATE_CHARS,
    MapConfig, Question, Statement,
};
use livingbrain_models::Models;
use livingbrain_models::jev::{self, JevConnector, JevKey, Source};
use serde_json::{Value, json};
use support::{FakeHttp, PLAIN, WORKSPACE, delete, get, put, send};

/// The workspace's own TypeSafe key: no test expects to see it again.
const TS_KEY: &str = "ts-live-WORKSPACESECRET9876";

/// What the fake classifier answers.
#[derive(Clone, Copy)]
enum Reply {
    Yes,
    Refused,
    Down,
}

/// A connector that records which key and model each classifier was
/// built over, and builds a classifier that answers `reply`.
struct FakeJev {
    deployment: bool,
    reply: Reply,
    built: Arc<Mutex<Vec<(String, String)>>>,
}

impl FakeJev {
    fn new(deployment: bool, reply: Reply) -> Arc<Self> {
        Arc::new(Self {
            deployment,
            reply,
            built: Arc::default(),
        })
    }

    fn built(&self) -> Vec<(String, String)> {
        self.built.lock().expect("lock").clone()
    }
}

impl JevConnector for FakeJev {
    fn deployment_key(&self) -> bool {
        self.deployment
    }

    fn default_model(&self) -> String {
        "jev-1.13.0".to_owned()
    }

    fn classifier(&self, key: JevKey<'_>, model: &str) -> Option<Arc<dyn Classifier>> {
        let key = match key {
            JevKey::Deployment if !self.deployment => return None,
            JevKey::Deployment => "deployment".to_owned(),
            JevKey::Workspace(key) => key.to_owned(),
        };
        self.built
            .lock()
            .expect("lock")
            .push((key, model.to_owned()));
        Some(Arc::new(FakeClassifier(self.reply, model.to_owned())))
    }
}

struct FakeClassifier(Reply, String);

#[async_trait]
impl Classifier for FakeClassifier {
    fn profile(&self) -> ClassifierProfile {
        ClassifierProfile::new(Calibration::Classifier, DEFAULT_MAX_STATE_CHARS)
    }

    async fn ask(
        &self,
        _state: &str,
        questions: &BTreeMap<String, Question>,
    ) -> Result<BTreeMap<String, Answer>, ClassifierError> {
        match self.0 {
            // The vendor's own words, which must never reach the member.
            Reply::Refused => Err(ClassifierError::Rejected(
                "invalid api key ts-live-WORKSPACESECRET9876".to_owned(),
            )),
            Reply::Down => Err(ClassifierError::Transient { retry_after: None }),
            Reply::Yes => Ok(questions
                .keys()
                .map(|id| {
                    let answer = Answer::noul(true, BTreeMap::from([("true".to_owned(), 0.93)]))
                        .with_model(self.1.clone());
                    (id.clone(), answer)
                })
                .collect()),
        }
    }
}

fn config() -> MapConfig {
    MapConfig::from_pairs([("MODEL_KEYS_SECRET", "a-test-encryption-secret-of-32-bytes")])
}

async fn effective(kit: &cratefield_testing::TestHarness, connector: &FakeJev) -> jev::Effective {
    jev::effective(&*kit.db, &config(), WORKSPACE, connector)
        .await
        .expect("the row reads")
}

async fn post(
    kit: &cratefield_testing::TestHarness,
    path: &str,
    cookie: &str,
    body: Value,
) -> support::Res {
    send(kit, Method::POST, path, cookie, Some(body)).await
}

#[test]
fn with_nothing_stored_the_deployment_key_serves_on_the_pinned_model() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;

        let res = get(&kit, "/v1/models/jev", &cookie).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let view = res.json();
        assert_eq!(view["enabled"], true);
        assert_eq!(view["source"], "deployment");
        assert_eq!(view["deployment_key"], true);
        assert_eq!(view["workspace_key"], Value::Null);
        assert_eq!(view["model"], "jev-1.13.0");
        assert_eq!(view["available"], true);

        let resolved = effective(&kit, &connector).await;
        assert_eq!(resolved.source, Source::Deployment);
        assert!(resolved.classifier.is_some());
        assert_eq!(
            connector.built(),
            vec![("deployment".to_owned(), "jev-1.13.0".to_owned())]
        );
    });
}

#[test]
fn a_workspace_key_wins_over_the_deployment_key_and_never_comes_back() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;

        let res = put(
            &kit,
            "/v1/models/jev",
            &cookie,
            json!({"api_key": TS_KEY, "model": "jev-2.0.0"}),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert!(
            !res.text().contains(TS_KEY),
            "the key came back: {}",
            res.text()
        );
        assert!(!res.text().contains("WORKSPACESECRET"), "{}", res.text());
        let view = res.json();
        assert_eq!(view["source"], "workspace");
        assert_eq!(view["workspace_key"], "…9876");
        assert_eq!(view["model"], "jev-2.0.0");
        assert_eq!(view["custom_model"], true);

        let read = get(&kit, "/v1/models/jev", &cookie).await;
        assert!(!read.text().contains(TS_KEY), "{}", read.text());

        // Sealed at rest: the row holds no plaintext key.
        let rows = kit
            .db
            .query(&Statement::new("SELECT key_ciphertext FROM jev_settings"))
            .await
            .expect("reads");
        let blob: Vec<u8> = rows
            .first()
            .and_then(|row| row.get("key_ciphertext"))
            .expect("a key");
        assert!(!String::from_utf8_lossy(&blob).contains("WORKSPACESECRET"));

        let resolved = effective(&kit, &connector).await;
        assert_eq!(resolved.source, Source::Workspace);
        assert_eq!(resolved.model, "jev-2.0.0");
        assert_eq!(
            connector.built(),
            vec![(TS_KEY.to_owned(), "jev-2.0.0".to_owned())]
        );

        // Removing it hands back to the deployment's key, on the chosen model.
        let res = delete(&kit, "/v1/models/jev/key", &cookie).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["source"], "deployment");
        assert_eq!(res.json()["workspace_key"], Value::Null);
        assert_eq!(effective(&kit, &connector).await.source, Source::Deployment);

        // A cleared model is the default again.
        let res = put(&kit, "/v1/models/jev", &cookie, json!({"model": ""})).await;
        assert_eq!(res.json()["model"], "jev-1.13.0");
        assert_eq!(res.json()["custom_model"], false);
    });
}

#[test]
fn the_switch_turns_jev_off_even_with_both_keys() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;
        put(&kit, "/v1/models/jev", &cookie, json!({"api_key": TS_KEY})).await;

        let res = put(&kit, "/v1/models/jev", &cookie, json!({"enabled": false})).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["enabled"], false);
        assert_eq!(res.json()["source"], "off");
        // The key is kept for when it is switched back on.
        assert_eq!(res.json()["workspace_key"], "…9876");

        let off = effective(&kit, &connector).await;
        assert_eq!(off.source, Source::Off);
        assert!(off.classifier.is_none());
        assert!(connector.built().is_empty(), "no classifier was built");

        let res = post(&kit, "/v1/models/jev/test", &cookie, json!({})).await;
        assert_eq!(res.status, StatusCode::CONFLICT, "{}", res.text());
        assert!(res.text().contains("models/jev-off"), "{}", res.text());

        put(&kit, "/v1/models/jev", &cookie, json!({"enabled": true})).await;
        assert_eq!(effective(&kit, &connector).await.source, Source::Workspace);
    });
}

#[test]
fn with_no_key_anywhere_jev_is_off() {
    pollster::block_on(async {
        let connector = FakeJev::new(false, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;
        let view = get(&kit, "/v1/models/jev", &cookie).await.json();
        assert_eq!(view["source"], "off");
        assert_eq!(view["deployment_key"], false);
        assert_eq!(effective(&kit, &connector).await.source, Source::Off);

        // A server with no TypeSafe adapter at all says so.
        let (kit, cookie) = support::signed_in(&FakeHttp::new()).await;
        let view = get(&kit, "/v1/models/jev", &cookie).await.json();
        assert_eq!(view["available"], false);
        assert_eq!(view["source"], "off");
    });
}

#[test]
fn only_an_owner_or_admin_changes_or_tests_jev() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, _) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;
        let plain = support::session(&kit, WORKSPACE, PLAIN);

        assert_eq!(
            get(&kit, "/v1/models/jev", &plain).await.status,
            StatusCode::OK
        );
        for res in [
            put(&kit, "/v1/models/jev", &plain, json!({"enabled": false})).await,
            put(&kit, "/v1/models/jev", &plain, json!({"api_key": TS_KEY})).await,
            delete(&kit, "/v1/models/jev/key", &plain).await,
            post(&kit, "/v1/models/jev/test", &plain, json!({})).await,
        ] {
            assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
        }
        assert_eq!(effective(&kit, &connector).await.source, Source::Deployment);
    });
}

#[test]
fn the_test_call_classifies_once_and_names_failures_without_the_vendor_text() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector.clone()))
                .await;

        let res = post(&kit, "/v1/models/jev/test", &cookie, json!({})).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["ok"], true);
        assert_eq!(body["source"], "deployment");
        assert_eq!(body["model"], "jev-1.13.0");
        assert_eq!(body["answer"], true);

        // A key and model sent with the test are tried, not stored.
        let res = post(
            &kit,
            "/v1/models/jev/test",
            &cookie,
            json!({"api_key": TS_KEY, "model": "jev-2.0.0"}),
        )
        .await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["source"], "workspace");
        assert!(!res.text().contains(TS_KEY));
        assert_eq!(
            connector.built().last(),
            Some(&(TS_KEY.to_owned(), "jev-2.0.0".to_owned()))
        );
        assert_eq!(
            get(&kit, "/v1/models/jev", &cookie).await.json()["workspace_key"],
            Value::Null
        );

        let refused = FakeJev::new(true, Reply::Refused);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(refused)).await;
        let res = post(
            &kit,
            "/v1/models/jev/test",
            &cookie,
            json!({"api_key": TS_KEY}),
        )
        .await;
        assert_eq!(
            res.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{}",
            res.text()
        );
        assert!(res.text().contains("models/jev-refused"), "{}", res.text());
        assert!(!res.text().contains("WORKSPACESECRET"), "{}", res.text());
        assert!(!res.text().contains("invalid api key"), "{}", res.text());

        let down = FakeJev::new(true, Reply::Down);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(down)).await;
        let res = post(&kit, "/v1/models/jev/test", &cookie, json!({})).await;
        assert_eq!(res.status, StatusCode::BAD_GATEWAY, "{}", res.text());
        assert!(
            res.text().contains("models/jev-unreachable"),
            "{}",
            res.text()
        );
    });
}

#[test]
fn a_malformed_model_or_key_is_refused() {
    pollster::block_on(async {
        let connector = FakeJev::new(true, Reply::Yes);
        let (kit, cookie) =
            support::signed_in_with(&FakeHttp::new(), Models::new().with_jev(connector)).await;
        for body in [
            json!({"model": "jev 1; drop"}),
            json!({"api_key": "   "}),
            json!({"model": "x".repeat(65)}),
        ] {
            let res = put(&kit, "/v1/models/jev", &cookie, body).await;
            assert_eq!(res.status, StatusCode::BAD_REQUEST, "{}", res.text());
        }
    });
}

#[test]
fn the_jev_routes_do_not_shadow_the_role_routes() {
    pollster::block_on(async {
        let (kit, cookie) = support::signed_in(&FakeHttp::new()).await;
        // `/jev` is not a role: `/{role}` still validates its own.
        let res = get(&kit, "/v1/models/main", &cookie).await;
        assert_eq!(res.status, StatusCode::NOT_FOUND, "{}", res.text());
        assert!(res.text().contains("models/managed"), "{}", res.text());
    });
}
