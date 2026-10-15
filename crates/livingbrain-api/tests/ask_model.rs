//! `POST /v1/ask` with a model: a scripted stand-in for the workspace's
//! connected model ([`AskModel`]) answers from the retrieved brief, and the
//! route checks what goes out and what comes back — citations, redaction,
//! the budget, the no-model fallback, and that nothing outside the asker's
//! scopes ever reaches the model.

mod common;

use std::sync::{Arc, Mutex};

use cratefield_core::axum::body::Body;
use cratefield_core::axum::http::{HeaderMap, Request, StatusCode};
use cratefield_core::{ModuleContext, Ports};
use livingbrain_access::Scope;
use livingbrain_api::{
    AskModel, DEFAULT_CONTEXT_CHARS_MAX, MAX_ANSWER_TOKENS, MAX_CONTEXT_CHARS, MODEL_FAILED,
    NO_MODEL, OVER_BUDGET, SessionAuth,
};
use livingbrain_mcp::{Asker, AuthError};
use livingbrain_models::invoke::{ChatReply, ChatRequest, InvokeError, Serving};
use serde_json::{Value, json};
use tower::ServiceExt;

use common::{TOKEN_A, WORKSPACE, fixture, fixture_with_ask_model, post, seed, user};

const KESTREL: &str =
    "---\ntitle: Kestrel routes\n---\nThe kestrel routes run at dawn near the harbour.";

/// A secret-shaped string the redactor removes (an AWS access key id).
const SECRET: &str = "AKIAIOSFODNN7EXAMPLE";

/// What the scripted model was asked: the system prompt, the user turn and
/// the completion ceiling.
#[derive(Clone, Debug)]
struct Seen {
    system: String,
    user: String,
    max_tokens: u32,
}

/// A model that answers every call with `reply` (or fails with `error`),
/// served as `serving`, and remembers what it was sent.
struct Scripted {
    serving: Option<Serving>,
    reply: Result<String, InvokeError>,
    seen: Mutex<Vec<Seen>>,
}

impl Scripted {
    fn answering(reply: &str) -> Arc<Self> {
        Arc::new(Self {
            serving: Some(serving("openai", "gpt-test")),
            reply: Ok(reply.to_owned()),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// The same, served by a model that reported a `window`-token context.
    fn with_window(reply: &str, window: u64) -> Arc<Self> {
        let mut serving = serving("zai", "glm-test");
        serving.context_size = Some(window);
        Arc::new(Self {
            serving: Some(serving),
            reply: Ok(reply.to_owned()),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> Vec<Seen> {
        self.seen.lock().expect("seen").clone()
    }
}

fn serving(provider: &str, model: &str) -> Serving {
    Serving {
        role: "main".to_owned(),
        provider: provider.to_owned(),
        model: model.to_owned(),
        context_size: None,
    }
}

#[async_trait::async_trait]
impl AskModel for Scripted {
    async fn serving(
        &self,
        _ctx: &ModuleContext,
        _workspace: &str,
    ) -> Result<Option<Serving>, InvokeError> {
        Ok(self.serving.clone())
    }

    async fn chat(
        &self,
        _ctx: &ModuleContext,
        _workspace: &str,
        request: &ChatRequest<'_>,
    ) -> Result<ChatReply, InvokeError> {
        self.seen.lock().expect("seen").push(Seen {
            system: request.system.to_owned(),
            user: request.user.to_owned(),
            max_tokens: request.max_tokens,
        });
        let text = self.reply.clone()?;
        let serving = self.serving.clone().ok_or(InvokeError::NoModel)?;
        Ok(ChatReply {
            text,
            prompt_tokens: 10,
            completion_tokens: 5,
            usage_reported: true,
            role: serving.role,
            provider: serving.provider,
            model: serving.model,
            runner: false,
        })
    }
}

fn ask(fixture: &common::Fixture, body: Value) -> Value {
    let response = post(fixture, TOKEN_A, "/v1/ask", body);
    assert_eq!(
        response.status,
        StatusCode::OK,
        "{}",
        common::body_text(&response)
    );
    response.json()
}

fn answer_of(body: &Value) -> &str {
    body["answer"].as_str().expect("an answer")
}

#[test]
fn the_model_answers_with_citations_to_the_sources_it_was_given() {
    let model = Scripted::answering("The kestrel routes run at dawn [1].");
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    let scope = common::scope_of(WORKSPACE, "user-a");

    let body = ask(
        &fx,
        json!({ "question": "kestrel routes run", "as_of": "2026-03" }),
    );
    assert_eq!(answer_of(&body), "The kestrel routes run at dawn [1].");
    assert_eq!(body["mode"], "model", "{body}");
    assert_eq!(body["model"]["provider"], "openai", "{body}");
    assert_eq!(body["dropped_citations"], json!([]), "{body}");
    // The shape every client reads is unchanged: [1] is citations[0].
    let citations = body["citations"].as_array().expect("citations");
    assert_eq!(citations[0]["title"], "Kestrel routes");
    assert_eq!(citations[0]["url"], common::page_url(&scope, "kestrel"));

    let calls = model.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    // The brief went out numbered, with the question and the as-of date.
    assert!(call.user.contains("[1] Kestrel routes"), "{}", call.user);
    assert!(
        call.user.contains("run at dawn near the harbour"),
        "{}",
        call.user
    );
    assert!(call.user.contains("kestrel routes run"), "{}", call.user);
    assert!(call.user.contains("As of: 2026-03"), "{}", call.user);
    // And the rules: cite provided sources only, say when they do not
    // answer, follow the temporal convention.
    assert!(
        call.system
            .contains("Use only labels that appear in the sources")
    );
    assert!(call.system.contains("do not answer it"));
    assert!(call.system.contains("Facts are temporal"));
    assert_eq!(call.max_tokens, MAX_ANSWER_TOKENS);
}

#[test]
fn a_model_that_finds_no_answer_in_the_sources_says_so() {
    let model = Scripted::answering("The pages you can read do not answer this.");
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let body = ask(&fx, json!({ "question": "What is the osprey budget?" }));
    // No page holds `osprey` or `budget` (the rest are stopwords), so there
    // are no sources and no model is asked.
    assert_eq!(
        answer_of(&body),
        "Nothing in the pages you can read mentions this."
    );
    assert_eq!(body["mode"], "sources");
    assert!(model.calls().is_empty(), "no sources, no call");

    // With sources that do not settle it, the model's honest answer stands.
    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert_eq!(
        answer_of(&body),
        "The pages you can read do not answer this."
    );
    assert_eq!(body["mode"], "model");
    assert_eq!(model.calls().len(), 1);
}

#[test]
fn a_citation_that_names_no_source_is_removed_and_flagged() {
    let model = Scripted::answering("At dawn [1]. Also at dusk [4]. See [1, 7].");
    let fx = fixture_with_ask_model(model);
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    let body = ask(&fx, json!({ "question": "kestrel" }));
    let answer = answer_of(&body);
    assert!(
        answer.starts_with("At dawn [1]. Also at dusk. See [1]."),
        "{answer}"
    );
    assert!(answer.contains("Removed 2 citation(s)"), "{answer}");
    assert_eq!(body["dropped_citations"], json!(["4", "7"]));
}

#[test]
fn what_goes_to_the_model_and_what_comes_back_are_redacted() {
    let model = Scripted::answering(&format!("The key is {SECRET} [1]."));
    let fx = fixture_with_ask_model(model.clone());
    seed(
        &fx,
        WORKSPACE,
        &user("user-a"),
        "kestrel",
        &format!("---\ntitle: Kestrel routes\n---\nThe kestrel deploy key is {SECRET}."),
    );

    let body = ask(&fx, json!({ "question": format!("kestrel {SECRET}") }));
    let call = &model.calls()[0];
    assert!(
        !call.user.contains(SECRET),
        "the context and question are redacted: {}",
        call.user
    );
    assert!(call.user.contains("kestrel deploy key"), "{}", call.user);
    let answer = answer_of(&body);
    assert!(!answer.contains(SECRET), "the answer is redacted: {answer}");
    assert!(answer.starts_with("The key is"), "{answer}");
}

#[test]
fn a_workspace_over_its_budget_gets_its_sources_and_no_call() {
    let model = Arc::new(Scripted {
        // A priced model, so the ledger's tokens have a cost.
        serving: Some(serving("deepseek", "deepseek-chat")),
        reply: Ok("never".to_owned()),
        seen: Mutex::new(Vec::new()),
    });
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    pollster::block_on(livingbrain_usage::record_model(
        &*fx.db(),
        fx.clock().now(),
        WORKSPACE,
        1_000_000_000,
        1_000_000_000,
    ))
    .expect("a ledger row");

    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert!(model.calls().is_empty(), "no call over budget");
    let answer = answer_of(&body);
    assert!(answer.starts_with(OVER_BUDGET), "{answer}");
    assert!(answer.contains("[1] Kestrel routes"), "{answer}");
    assert_eq!(body["mode"], "sources");
}

#[test]
fn with_no_model_connected_the_sources_come_back_labelled() {
    // The composition's own model: nothing is connected in this workspace.
    let fx = fixture();
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    let body = ask(&fx, json!({ "question": "kestrel" }));
    let answer = answer_of(&body);
    assert!(answer.starts_with(NO_MODEL), "{answer}");
    assert!(
        answer.contains("The kestrel routes run at dawn"),
        "{answer}"
    );
    assert_eq!(body["mode"], "sources");
    assert_eq!(body["model"], Value::Null);
    assert_eq!(body["citations"].as_array().map(Vec::len), Some(1));
}

#[test]
fn a_failed_model_call_falls_back_to_the_sources() {
    let model = Arc::new(Scripted {
        serving: Some(serving("openai", "gpt-test")),
        reply: Err(InvokeError::Http(Some(500))),
        seen: Mutex::new(Vec::new()),
    });
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert!(answer_of(&body).starts_with(MODEL_FAILED), "{body}");
    assert_eq!(model.calls().len(), 1);
}

/// The fallback says why: the provider's status and (safe) error body, so a
/// failing Ask is diagnosable from its own answer.
#[test]
fn a_failed_model_call_says_why_it_fell_back() {
    let model = Arc::new(Scripted {
        serving: Some(serving("zhipu-glm-en", "glm-5.3-flash")),
        reply: Err(InvokeError::Status(livingbrain_models::invoke::Upstream {
            status: 429,
            body: r#"{"error":{"code":"1302","message":"Rate limit reached for requests"}}"#
                .to_owned(),
            retry_after_secs: None,
        })),
        seen: Mutex::new(Vec::new()),
    });
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert!(answer_of(&body).starts_with(MODEL_FAILED), "{body}");
    assert_eq!(body["mode"], "sources");
    let reason = body["fallback_reason"].as_str().expect("a reason");
    assert!(reason.starts_with("model-http 429: "), "{reason}");
    assert!(reason.contains("Rate limit reached"), "{reason}");
}

#[test]
fn the_model_never_sees_a_page_the_asker_cannot_read() {
    let model = Scripted::answering("Dawn [1].");
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    seed(
        &fx,
        WORKSPACE,
        &user("user-b"),
        "kestrel-b",
        "---\ntitle: B's kestrel\n---\nThe kestrel plan user-b keeps private: osprey.",
    );
    seed(
        &fx,
        common::OTHER_WORKSPACE,
        &Scope::Shared,
        "kestrel-other",
        "---\ntitle: Elsewhere\n---\nAnother workspace's kestrel: merlin.",
    );

    let body = ask(&fx, json!({ "question": "kestrel" }));
    let call = &model.calls()[0];
    assert!(call.user.contains("run at dawn"), "{}", call.user);
    for private in ["osprey", "B's kestrel", "merlin", "Elsewhere"] {
        assert!(
            !call.user.contains(private),
            "{private} leaked to the model: {}",
            call.user
        );
    }
    assert_eq!(body["citations"].as_array().map(Vec::len), Some(1));
}

#[test]
fn one_call_s_context_is_capped() {
    let model = Scripted::answering("Long [1].");
    let fx = fixture_with_ask_model(model.clone());
    for n in 0..5 {
        seed(
            &fx,
            WORKSPACE,
            &user("user-a"),
            &format!("kestrel-{n}"),
            &format!(
                "---\ntitle: Kestrel {n}\n---\nkestrel {}",
                "word ".repeat(10_000)
            ),
        );
    }
    let body = ask(&fx, json!({ "question": "kestrel" }));
    let call = &model.calls()[0];
    assert!(
        call.user.chars().count() <= MAX_CONTEXT_CHARS + 500,
        "{} chars",
        call.user.chars().count()
    );
    // The person still gets the full brief's citations.
    assert_eq!(body["citations"].as_array().map(Vec::len), Some(5));
}

/// The production failure: a natural-language question, mostly stopwords,
/// found nothing although a synced session named every word that matters.
/// It must find the session — and only the asker's: another member's page
/// naming every word never becomes a candidate.
#[test]
fn a_natural_language_question_finds_the_session_it_is_about() {
    let model = Scripted::with_window("Two issues: a stale lockfile [1].", 1_000_000);
    let fx = fixture_with_ask_model(model.clone());
    seed(
        &fx,
        WORKSPACE,
        &user("user-a"),
        "agent-log-claude-code-3f2a9c1e",
        "---\ntitle: claude_code session 3f2a9c1e\nproject: earthos\n---\n\
         Coding agent session: claude_code in earthos.\n\n- Repo: ~/code/earthos\n\n\
         ## Conversation\n\n**Assistant:** Reviewed the codebase. Two issues: a stale \
         lockfile and a flaky test.",
    );
    seed(
        &fx,
        WORKSPACE,
        &user("user-b"),
        "earthos-issues",
        "---\ntitle: EarthOS issues in the codebase\n---\nb-only-kingfisher earthos issues codebase",
    );

    let body = ask(
        &fx,
        json!({ "question": "What issues were found in the EarthOS codebase?" }),
    );
    assert_eq!(body["mode"], "model", "{body}");
    let citations = body["citations"].as_array().expect("citations");
    assert_eq!(citations.len(), 1, "{body}");
    assert_eq!(citations[0]["title"], "claude_code session 3f2a9c1e");
    let call = &model.calls()[0];
    assert!(
        call.user.contains("a stale lockfile and a flaky test"),
        "{}",
        call.user
    );
    assert!(!call.user.contains("b-only-kingfisher"), "{}", call.user);

    // The same question through search, and a question of only stopwords.
    let search = common::get(
        &fx,
        TOKEN_A,
        "/v1/search?q=What%20issues%20were%20found%20in%20the%20EarthOS%20codebase%3F",
    );
    assert_eq!(search.status, StatusCode::OK);
    let results = search.json();
    assert_eq!(
        results["results"][0]["title"], "claude_code session 3f2a9c1e",
        "{results}"
    );
    let body = ask(&fx, json!({ "question": "What were they?" }));
    assert_eq!(
        answer_of(&body),
        "Nothing in the pages you can read mentions this."
    );
}

/// A model that reported a large window gets a brief sized to it — more than
/// the fixed budget of a model that reported none, never past the ceiling.
#[test]
fn the_brief_scales_with_the_model_s_window() {
    let model = Scripted::with_window("Long [1].", 1_000_000);
    let fx = fixture_with_ask_model(model.clone());
    for n in 0..5 {
        seed(
            &fx,
            WORKSPACE,
            &user("user-a"),
            &format!("kestrel-{n}"),
            &format!(
                "---\ntitle: Kestrel {n}\n---\nkestrel {}",
                "word ".repeat(60_000)
            ),
        );
    }
    ask(&fx, json!({ "question": "kestrel" }));
    let sent = model.calls()[0].user.chars().count();
    assert!(sent > MAX_CONTEXT_CHARS * 2, "{sent} chars");
    assert!(sent <= DEFAULT_CONTEXT_CHARS_MAX + 500, "{sent} chars");
}

/// A long page cut to its share contributes the passage that mentions the
/// question, not only its opening.
#[test]
fn a_long_page_sends_the_passage_that_mentions_the_question() {
    let model = Scripted::answering("Found it [1].");
    let fx = fixture_with_ask_model(model.clone());
    // Filler without numbered lines: the blind index keeps a page's first
    // 2,000 distinct words, and the point here is the context cut.
    let mut lines: Vec<String> = (0..4_000)
        .map(|_| "routine output, nothing to see here".to_owned())
        .collect();
    lines[3_500] = "The osprey migration stalled on the lockfile.".to_owned();
    seed(
        &fx,
        WORKSPACE,
        &user("user-a"),
        "long-session",
        &format!("---\ntitle: A long session\n---\n{}", lines.join("\n")),
    );
    ask(
        &fx,
        json!({ "question": "Why did the osprey migration stall?" }),
    );
    let call = &model.calls()[0];
    assert!(
        call.user
            .contains("The osprey migration stalled on the lockfile."),
        "the relevant passage was not sent"
    );
    assert!(call.user.chars().count() <= MAX_CONTEXT_CHARS + 500);
}

/// A session resolver for the web app's cookie: any `cookie` header names
/// user-a, and the origin check passes only the app's own origin.
struct Cookie;

#[async_trait::async_trait]
impl SessionAuth for Cookie {
    async fn session(&self, _ports: &Ports, headers: &HeaderMap) -> Result<Asker, AuthError> {
        if headers.contains_key("cookie") {
            Ok(Asker::member(WORKSPACE, "user-a", None))
        } else {
            Err(AuthError::Rejected)
        }
    }

    fn same_origin(&self, headers: &HeaderMap) -> Result<(), String> {
        match headers.get("origin").and_then(|v| v.to_str().ok()) {
            Some(common::APP_BASE) => Ok(()),
            _ => Err("cross-site".to_owned()),
        }
    }
}

fn cookie_ask(fx: &common::Fixture, origin: &str) -> StatusCode {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/ask")
        .header("content-type", "application/json")
        .header("cookie", "lb_session=x")
        .header("origin", origin)
        .body(Body::from(json!({ "question": "kestrel" }).to_string()))
        .expect("a request");
    pollster::block_on(fx.router.clone().oneshot(request))
        .expect("infallible")
        .status()
}

#[test]
fn a_cookie_ask_from_another_origin_is_refused_before_any_model_call() {
    let model = Scripted::answering("Dawn [1].");
    let fx = common::fixture_with_session(model.clone(), Arc::new(Cookie));
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);

    assert_eq!(
        cookie_ask(&fx, "https://evil.example"),
        StatusCode::FORBIDDEN
    );
    assert!(model.calls().is_empty());
    assert_eq!(cookie_ask(&fx, common::APP_BASE), StatusCode::OK);
    assert_eq!(model.calls().len(), 1);
}

/// The workspace's own budgets (Settings → Usage & limits) are what an ask
/// runs under: its context size caps the brief however large the model's
/// window, its answer length is the completion ceiling, and its spend limit
/// decides "over budget" — each in place of the deployment's.
#[test]
fn an_ask_runs_under_the_workspace_s_own_budgets() {
    let model = Scripted::with_window("Long [1].", 1_000_000);
    let fx = fixture_with_ask_model(model.clone());
    for n in 0..5 {
        seed(
            &fx,
            WORKSPACE,
            &user("user-a"),
            &format!("kestrel-{n}"),
            &format!(
                "---\ntitle: Kestrel {n}\n---\nkestrel {}",
                "word ".repeat(60_000)
            ),
        );
    }
    let own = livingbrain_usage::Budgets {
        ask_context_chars: Some(20_000),
        ask_max_output_tokens: Some(2_048),
        ..livingbrain_usage::Budgets::default()
    };
    pollster::block_on(livingbrain_usage::save_budgets(
        &*fx.db(),
        fx.clock().now(),
        WORKSPACE,
        &own,
    ))
    .expect("the budgets save");
    ask(&fx, json!({ "question": "kestrel" }));
    let call = &model.calls()[0];
    let sent = call.user.chars().count();
    assert!(
        sent <= 20_000 + 500,
        "{sent} chars, under the workspace's 20k"
    );
    assert!(sent > 10_000, "{sent} chars");
    assert_eq!(call.max_tokens, 2_048, "the workspace's answer length");

    // A spend limit of zero is over at the first priced token.
    let model = Arc::new(Scripted {
        serving: Some(serving("deepseek", "deepseek-chat")),
        reply: Ok("never".to_owned()),
        seen: Mutex::new(Vec::new()),
    });
    let fx = fixture_with_ask_model(model.clone());
    seed(&fx, WORKSPACE, &user("user-a"), "kestrel", KESTREL);
    pollster::block_on(livingbrain_usage::record_model(
        &*fx.db(),
        fx.clock().now(),
        WORKSPACE,
        10_000,
        10_000,
    ))
    .expect("a ledger row");
    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert_eq!(body["mode"], "model", "well inside the default $3");
    let zero = livingbrain_usage::Budgets {
        spend_cents_monthly: Some(0),
        ..livingbrain_usage::Budgets::default()
    };
    pollster::block_on(livingbrain_usage::save_budgets(
        &*fx.db(),
        fx.clock().now(),
        WORKSPACE,
        &zero,
    ))
    .expect("the budgets save");
    let body = ask(&fx, json!({ "question": "kestrel" }));
    assert!(answer_of(&body).starts_with(OVER_BUDGET), "{body}");
    assert_eq!(model.calls().len(), 1, "no second call once over");
}
