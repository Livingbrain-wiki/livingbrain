//! `invoke::chat`: a job runs on the connection a member connected, with
//! the key that connection stored, and its usage lands in the ledger.

mod support;

use std::time::Duration;

use cratefield_core::{Clock, MapConfig};
use cratefield_testing::FixedClock;
use livingbrain_models::invoke::{self, ChatRequest, InvokeError};
use serde_json::json;
use support::{FakeHttp, KEY, WORKSPACE, put};
use time::OffsetDateTime;

fn request() -> ChatRequest<'static> {
    ChatRequest {
        system: "You compile wiki pages.",
        user: "the data",
        max_tokens: 64,
        json: true,
        timeout: Duration::from_secs(5),
    }
}

fn config() -> MapConfig {
    MapConfig::from_pairs([("MODEL_KEYS_SECRET", "a-test-encryption-secret-of-32-bytes")])
}

fn clock() -> FixedClock {
    FixedClock(OffsetDateTime::from_unix_timestamp(support::NOW).expect("an instant"))
}

/// With nothing connected there is no model, and nothing is sent.
#[test]
fn nothing_connected_is_no_model() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _cookie) = support::signed_in(&http).await;
        let clock = clock();
        let error = invoke::chat(
            &*kit.db,
            &http,
            &config(),
            &clock as &dyn Clock,
            WORKSPACE,
            "main",
            &request(),
        )
        .await
        .expect_err("nothing serves main");
        assert_eq!(error, InvokeError::NoModel);
        assert!(http.sent().is_empty(), "nothing went out");
        assert!(
            invoke::serving(&*kit.db, WORKSPACE, "main")
                .await
                .expect("a read")
                .is_none()
        );
    });
}

/// A connected Main serves a job: the stored key travels in the header the
/// provider expects, the system prompt and the data go out, and the reply's
/// text comes back with the connection that served it.
#[test]
fn a_job_runs_on_the_connected_model_with_its_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        http.calling_tools(false);
        let (kit, cookie) = support::signed_in(&http).await;
        let connected = put(
            &kit,
            "/v1/models/main",
            &cookie,
            json!({"provider": "custom", "base_url": "https://llm.example.com/v1",
                   "api_key": KEY, "model": "some-model"}),
        )
        .await;
        assert_eq!(connected.status.as_u16(), 200, "{}", connected.text());
        let before = http.sent().len();
        // The connect probe asks for its own minute, past the port's 30 s
        // ceiling, on every request it sent to the model host.
        assert!(before > 0);
        for probe in &http.sent()[..before] {
            assert_eq!(
                probe.deadline,
                Some(Duration::from_secs(60)),
                "{}",
                probe.uri
            );
        }

        let clock = clock();
        let reply = invoke::chat(
            &*kit.db,
            &http,
            &config(),
            &clock as &dyn Clock,
            WORKSPACE,
            "research",
            &request(),
        )
        .await
        .expect("the call answers");
        assert_eq!(reply.role, "main", "an unset role falls back to Main");
        assert_eq!(reply.model, "some-model");
        assert_eq!(reply.text, "{\"ok\":true}");

        let sent = http.sent();
        let call = &sent[before];
        assert!(call.uri.ends_with("/v1/chat/completions"), "{}", call.uri);
        assert_eq!(
            call.authorization.as_deref(),
            Some(format!("Bearer {KEY}").as_str())
        );
        assert!(call.body.contains("You compile wiki pages."));
        assert!(call.body.contains("json_object"));
        // The caller's timeout travels as the call's own deadline.
        assert_eq!(call.deadline, Some(Duration::from_secs(5)));
        let serving = invoke::serving(&*kit.db, WORKSPACE, "triage")
            .await
            .expect("a read")
            .expect("Main serves triage");
        assert_eq!(serving.provider, "custom");
    });
}

/// A stored key never reaches an error: a provider failure is a status.
#[test]
fn an_error_names_no_key() {
    let error = InvokeError::Http(Some(401));
    assert!(!error.to_string().contains(KEY));
    assert_eq!(error.variant(), "http");
}

/// Connects Main to Z.AI's GLM-5.3-Flash on the Anthropic wire, as
/// production is, and answers the request count before any job runs.
async fn glm(http: &FakeHttp) -> (cratefield_testing::TestHarness, usize) {
    http.calling_tools(false);
    let (kit, cookie) = support::signed_in(http).await;
    let connected = put(
        &kit,
        "/v1/models/main",
        &cookie,
        json!({"provider": "zhipu-glm-en", "api_key": KEY, "model": "glm-5.3-flash"}),
    )
    .await;
    assert_eq!(connected.status.as_u16(), 200, "{}", connected.text());
    let before = http.sent().len();
    (kit, before)
}

async fn run(
    kit: &cratefield_testing::TestHarness,
    http: &FakeHttp,
) -> Result<invoke::ChatReply, InvokeError> {
    let clock = clock();
    invoke::chat(
        &*kit.db,
        http,
        &config(),
        &clock as &dyn Clock,
        WORKSPACE,
        "main",
        &request(),
    )
    .await
}

/// The production failure: Z.AI refuses a compile with 429 and code 1302.
/// The error carries the status, the provider's body and its Retry-After —
/// and never the key, even when the provider echoes it back.
#[test]
fn a_provider_refusal_carries_its_status_and_body_but_not_the_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        let body = format!(
            r#"{{"type":"error","error":{{"type":"rate_limit_error","code":"1302","message":"[1302][Rate limit reached for requests]"}},"echo":"{KEY}"}}"#
        );
        http.failing_chat(429, &body, Some("17"));
        let error = run(&kit, &http).await.expect_err("refused");
        assert!(error.rate_limited());
        assert_eq!(error.retry_after_secs(), Some(17));
        assert_eq!(error.variant(), "http");
        let reason = error.reason();
        assert!(reason.starts_with("http 429: {"), "{reason}");
        assert!(
            reason.contains("Rate limit reached for requests"),
            "{reason}"
        );
        assert!(!reason.contains(KEY), "{reason}");
        assert!(!error.to_string().contains(KEY));
        assert!(!format!("{error:?}").contains(KEY));
        // Nothing was answered, so nothing is in the ledger as spent.
    });
}

/// A long body is cut, on one line.
#[test]
fn a_long_error_body_is_cut_to_one_line() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        let long = format!("{{\"error\":\"{}\n{}\"}}", "x".repeat(400), "y".repeat(400));
        http.failing_chat(400, &long, None);
        let error = run(&kit, &http).await.expect_err("refused");
        assert!(!error.rate_limited());
        let InvokeError::Status(upstream) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(upstream.status, 400);
        assert!(upstream.body.chars().count() <= invoke::MAX_DETAIL_CHARS);
        assert!(!upstream.body.contains('\n'));
        assert!(upstream.body.ends_with('…'));
    });
}

/// A transport failure says what failed, scrubbed of the key.
#[test]
fn a_transport_failure_is_named_without_the_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.unreachable_chat(&format!("connection reset; Authorization: Bearer {KEY}"));
        let error = run(&kit, &http).await.expect_err("unreachable");
        let reason = error.reason();
        assert!(
            reason.starts_with("http: transport: connection reset"),
            "{reason}"
        );
        assert!(!reason.contains(KEY), "{reason}");
    });
}

/// A forced-thinking model that spends the whole ceiling thinking is named
/// as such, not as a bad response.
#[test]
fn thinking_through_the_whole_budget_is_exhausted() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.exhausting();
        let error = run(&kit, &http).await.expect_err("no answer");
        assert_eq!(error, InvokeError::Exhausted);
        assert_eq!(error.reason(), "max-tokens");
    });
}

/// The compile request Z.AI gets: the Anthropic Messages shape and nothing
/// a GLM-5.3 endpoint might refuse — no thinking switch, no sampling knobs,
/// no `response_format`, no `tool_choice`, the system prompt as a string.
#[test]
fn the_glm_request_carries_only_what_the_messages_api_takes() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, before) = glm(&http).await;
        http.thinking_first();
        let reply = run(&kit, &http).await.expect("answers");
        assert_eq!(reply.text, "{\"ok\":true}");
        let sent = &http.sent()[before];
        assert!(sent.uri.ends_with("/v1/messages"), "{}", sent.uri);
        let body: serde_json::Value = serde_json::from_str(&sent.body).expect("json");
        let mut keys: Vec<&str> = body
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        // `stream`: GLM-5.3 thinks before it answers, and a reply that sends
        // nothing for 100 s is cut by Z.AI's front with a 524.
        assert_eq!(
            keys,
            ["max_tokens", "messages", "model", "stream", "system"]
        );
        assert!(body["system"].is_string());
        assert_eq!(body["stream"], true);
    });
}

/// A GLM-5.3 stream as Z.AI sends it: thinking first, then the answer,
/// usage on `message_delta`.
fn glm_stream(answer: &str, stop: &str) -> String {
    let text = serde_json::to_string(answer).expect("json");
    format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":0,\"output_tokens\":0}}}}}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":0,\"content_block\":{{\"type\":\"thinking\",\"thinking\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"thinking_delta\",\"thinking\":\"Reason first.\"}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":0}}\n\n\
         event: content_block_start\ndata: {{\"type\":\"content_block_start\",\"index\":1,\"content_block\":{{\"type\":\"text\",\"text\":\"\"}}}}\n\n\
         event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":1,\"delta\":{{\"type\":\"text_delta\",\"text\":{text}}}}}\n\n\
         event: content_block_stop\ndata: {{\"type\":\"content_block_stop\",\"index\":1}}\n\n\
         event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{stop}\"}},\"usage\":{{\"input_tokens\":120,\"output_tokens\":30}}}}\n\n\
         event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    )
}

/// A client without the streaming seam buffers the whole stream: the
/// answer is read from the events, the thinking skipped, the usage counted.
#[test]
fn a_buffered_stream_reads_as_its_answer_and_usage() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.streaming_chat(&glm_stream("{\"pages\":[]}", "end_turn"));
        let reply = run(&kit, &http).await.expect("answers");
        assert_eq!(reply.text, "{\"pages\":[]}");
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (120, 30));
        assert!(reply.usage_reported);
    });
}

/// A stream that thought through the whole ceiling is exhausted.
#[test]
fn a_stream_cut_at_the_ceiling_is_exhausted() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        http.streaming_chat(&glm_stream("", "max_tokens"));
        assert_eq!(
            run(&kit, &http).await.expect_err("no answer"),
            InvokeError::Exhausted
        );
    });
}

/// An `error` event mid-stream is the provider's refusal: its status and
/// body, made safe like any other.
#[test]
fn an_error_mid_stream_is_a_status_without_the_key() {
    pollster::block_on(async {
        let http = FakeHttp::new();
        let (kit, _) = glm(&http).await;
        let full = glm_stream("never", "end_turn");
        let cut = &full[..full.find("event: content_block_start").expect("cut")];
        http.streaming_chat(&format!(
            "{cut}event: error\ndata: {{\"type\":\"error\",\"error\":{{\"type\":\"overloaded_error\",\"message\":\"Overloaded {KEY}\"}}}}\n\n"
        ));
        let error = run(&kit, &http).await.expect_err("failed");
        let InvokeError::Status(upstream) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(upstream.status, 529);
        assert!(upstream.body.contains("Overloaded"), "{}", upstream.body);
        assert!(!error.reason().contains(KEY), "{}", error.reason());
    });
}

/// Where the venture wired the streaming seam, a model call goes through
/// it: asked as a stream, signed with the key, its events read chunk by
/// chunk (here seven bytes at a time) into the same reply.
#[test]
fn the_streaming_seam_carries_the_call_chunk_by_chunk() {
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use bytes::Bytes;
    use cratefield_core::axum::http;
    use cratefield_core::{HttpClient, HttpError};
    use livingbrain_models::deadline::ModelDeadlineHttpClient;
    use livingbrain_models::stream::{ChunkStream, StreamHead, StreamingHttp};

    struct Chunks(Vec<Bytes>);

    #[async_trait]
    impl ChunkStream for Chunks {
        async fn next_chunk(&mut self) -> Option<Result<Bytes, HttpError>> {
            (!self.0.is_empty()).then(|| Ok(self.0.remove(0)))
        }
    }

    #[derive(Default)]
    struct Streaming {
        seen: Mutex<Vec<(String, Option<String>, String)>>,
    }

    #[async_trait]
    impl StreamingHttp for Streaming {
        async fn open(&self, request: http::Request<Bytes>) -> Result<StreamHead, HttpError> {
            self.seen.lock().expect("lock").push((
                request.uri().to_string(),
                request
                    .headers()
                    .get("authorization")
                    .or_else(|| request.headers().get("x-api-key"))
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                String::from_utf8_lossy(request.body()).into_owned(),
            ));
            let events = glm_stream("{\"pages\":[\"é\"]}", "end_turn").into_bytes();
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("text/event-stream"),
            );
            Ok(StreamHead {
                status: http::StatusCode::OK,
                headers,
                body: Box::new(Chunks(
                    events.chunks(7).map(Bytes::copy_from_slice).collect(),
                )),
            })
        }
    }

    pollster::block_on(async {
        let fake = FakeHttp::new();
        let (kit, before) = glm(&fake).await;
        let streaming = Arc::new(Streaming::default());
        let clock = clock();
        let client = ModelDeadlineHttpClient::new(
            Arc::new(fake.clone()) as Arc<dyn HttpClient>,
            Arc::new(clock.clone()) as Arc<dyn Clock>,
        )
        .with_streaming(Arc::clone(&streaming) as Arc<dyn StreamingHttp>);
        let reply = invoke::chat(
            &*kit.db,
            &client,
            &config(),
            &clock as &dyn Clock,
            WORKSPACE,
            "main",
            &request(),
        )
        .await
        .expect("answers");
        assert_eq!(reply.text, "{\"pages\":[\"é\"]}");
        assert_eq!((reply.prompt_tokens, reply.completion_tokens), (120, 30));
        // The completion went through the seam, not the buffered client.
        assert!(
            fake.sent()[before..]
                .iter()
                .all(|sent| !sent.uri.ends_with("/messages"))
        );
        let seen = streaming.seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 1);
        let (uri, key, body) = &seen[0];
        assert!(uri.ends_with("/v1/messages"), "{uri}");
        assert!(key.as_deref().is_some_and(|k| k.ends_with(KEY)), "signed");
        assert!(body.contains("\"stream\":true"), "{body}");
    });
}
