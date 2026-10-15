//! How a model call reads its reply as it arrives.
//!
//! A model call that asks for `"stream": true` goes out through the
//! `HttpClient` port's own streaming send (`cratefield-core` 0.9,
//! [`HttpClient::send_streaming`]) under a [`StreamPolicy`]: the head is
//! bounded by the port's buffered deadline (at most 30 s — a provider that
//! streams answers its headers at once), each gap between chunks by
//! [`IDLE_GAP`], and the whole body by the call's own deadline capped at
//! [`MAX_STREAM_DEADLINE`]. Those bounds are enforced by the runtime's
//! `BoundedHttpClient`, which every runtime wires around its transport; this
//! module only asks for them and reads what arrives.
//!
//! The reading is [`receive`]'s: the events go through
//! [`crate::sse::Accumulator`] as they come, so a long thinking stream is
//! never held in memory — only the answer is — and the result is handed back
//! as the buffered response [`crate::invoke::chat`] reads any reply as.

use std::time::Duration;

use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpClient, HttpError, MAX_STREAM_BYTES, StreamPolicy};
use futures_util::StreamExt;

use crate::catalog::Wire;
use crate::sse::{Accumulator, Failure};

/// The longest a stream may go without a byte. A thinking model streams
/// its reasoning token by token and providers send `ping`s, but GLM's
/// forced thinking can pause for a minute and more between deltas, so this
/// sits past the port's 60 s default — and under Cloudflare's 100 s
/// origin-idle cut, which the provider's own front would hit first anyway.
pub const IDLE_GAP: Duration = Duration::from_secs(90);

/// The longest a streamed model call may take in total, whatever its own
/// deadline asks. A cron invocation's wall is 15 minutes; a request route
/// runs while its client stays connected. Under the port's own 30-minute
/// ceiling (`cratefield_core::MAX_STREAM_TOTAL_TIMEOUT`).
pub const MAX_STREAM_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How much of a refused stream's body is read for its error.
const MAX_ERROR_BYTES: usize = 4 * 1024;

/// The [`StreamPolicy`] a streamed model call asks for: [`IDLE_GAP`]
/// between chunks, `deadline` (capped at [`MAX_STREAM_DEADLINE`]) for the
/// whole body. The raw byte cap is the port's: what is held is bounded by
/// the [`Accumulator`]'s own answer cap, not by the thinking streamed past.
#[must_use]
pub fn policy(deadline: Duration) -> StreamPolicy {
    StreamPolicy {
        idle_timeout: IDLE_GAP,
        total_timeout: deadline.min(MAX_STREAM_DEADLINE),
        max_bytes: MAX_STREAM_BYTES,
    }
}

/// Sends `request` (which carries its [`StreamPolicy`], see [`policy`])
/// through `http`'s streaming send and reads it back into a buffered
/// response [`crate::invoke::chat`] reads as it reads any other:
///
/// - a non-2xx head: that status with the first [`MAX_ERROR_BYTES`] of its
///   body (the provider's error);
/// - a 2xx `text/event-stream`: the events read as they arrive into the
///   wire's non-streaming JSON shape — or, for an `error` event or a
///   stream that broke off, the status and body of a [`Failure`];
/// - any other 2xx (a provider that ignored `"stream": true`): the body
///   as it came, held to `max_bytes`.
///
/// # Errors
///
/// [`HttpError::DeadlineExceeded`] past the head's deadline or the whole
/// body's, [`HttpError::Transport`] for an idle gap (named as a stall) or
/// a transport failure, [`HttpError::ResponseTooLarge`] past `max_bytes`.
pub(crate) async fn receive(
    http: &dyn HttpClient,
    request: http::Request<Bytes>,
    wire: Wire,
    max_bytes: usize,
) -> Result<http::Response<Bytes>, HttpError> {
    let idle = StreamPolicy::of_request(&request).idle_timeout;
    let total = StreamPolicy::of_request(&request).total_timeout;
    let (parts, mut body) = http.send_streaming(request).await?.into_parts();
    let status = parts.status;
    let event_stream = parts
        .headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));
    // A quiet gap is a stalled upstream, and says so; the whole-body
    // ceiling stays the deadline it is.
    let stalled = |error: HttpError| match error {
        HttpError::DeadlineExceeded { after } if after == idle && after != total => {
            HttpError::Transport(format!(
                "the model stream stalled: no bytes for {}s",
                after.as_secs()
            ))
        }
        other => other,
    };

    if !status.is_success() || !event_stream {
        let cap = if status.is_success() {
            max_bytes
        } else {
            MAX_ERROR_BYTES
        };
        let mut buffered = Vec::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(stalled)?;
            let room = cap.saturating_sub(buffered.len());
            if chunk.len() > room {
                if status.is_success() {
                    return Err(HttpError::ResponseTooLarge { limit: max_bytes });
                }
                buffered.extend_from_slice(&chunk[..room]);
                break;
            }
            buffered.extend_from_slice(&chunk);
        }
        return respond(status, Some(&parts.headers), Bytes::from(buffered));
    }

    let mut accumulator = Accumulator::new(wire, max_bytes);
    let completion = loop {
        match body.next().await {
            Some(chunk) => {
                if let Err(failure) = accumulator.feed(&chunk.map_err(stalled)?) {
                    return failed(&failure);
                }
            }
            None => match accumulator.finish() {
                Ok(completion) => break completion,
                Err(failure) => return failed(&failure),
            },
        }
    };
    respond(
        status,
        None,
        Bytes::from(completion.native_json(wire).to_string()),
    )
}

/// A [`Failure`] as the response [`crate::invoke::chat`] reads a provider's
/// refusal from.
fn failed(failure: &Failure) -> Result<http::Response<Bytes>, HttpError> {
    let status =
        http::StatusCode::from_u16(failure.status).unwrap_or(http::StatusCode::BAD_GATEWAY);
    respond(status, None, Bytes::from(failure.body.clone()))
}

fn respond(
    status: http::StatusCode,
    headers: Option<&http::HeaderMap>,
    body: Bytes,
) -> Result<http::Response<Bytes>, HttpError> {
    let mut builder = http::Response::builder().status(status);
    match headers {
        Some(headers) => {
            if let Some(out) = builder.headers_mut() {
                for (name, value) in headers {
                    if name != http::header::CONTENT_LENGTH {
                        out.append(name, value.clone());
                    }
                }
            }
        }
        None => {
            builder = builder.header(http::header::CONTENT_TYPE, "application/json");
        }
    }
    builder
        .body(body)
        .map_err(|_| HttpError::Transport("the streamed response could not be rebuilt".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};

    use async_trait::async_trait;
    use cratefield_core::{BoundedHttpClient, ByteStream, Clock, HttpPolicy};
    use time::OffsetDateTime;

    type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

    /// A clock whose timeout polls the future once: ready is an answer,
    /// pending is the bound lapsing. `now` advances by `step` per call.
    struct OnePoll {
        now: Mutex<OffsetDateTime>,
        step: Duration,
        bounds: Mutex<Vec<Duration>>,
    }

    impl OnePoll {
        fn new(step: Duration) -> Arc<Self> {
            Arc::new(Self {
                now: Mutex::new(OffsetDateTime::UNIX_EPOCH),
                step,
                bounds: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl Clock for OnePoll {
        fn now(&self) -> OffsetDateTime {
            let mut now = self.now.lock().expect("lock");
            let current = *now;
            *now += self.step;
            current
        }

        async fn timeout_any(
            &self,
            mut fut: BoxFuture<Box<dyn Any + Send>>,
            after: Duration,
        ) -> Option<Box<dyn Any + Send>> {
            self.bounds.lock().expect("lock").push(after);
            match fut.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                Poll::Ready(value) => Some(value),
                Poll::Pending => None,
            }
        }
    }

    /// One scripted chunk: bytes, a transport error, or a stall.
    enum Step {
        Bytes(&'static str),
        Fail,
        Stall,
    }

    /// A transport that streams a scripted body; `send` is never used.
    struct FakeStreaming {
        status: u16,
        content_type: &'static str,
        steps: Mutex<Option<Vec<Step>>>,
        policies: Mutex<Vec<StreamPolicy>>,
    }

    impl FakeStreaming {
        fn new(status: u16, content_type: &'static str, steps: Vec<Step>) -> Self {
            Self {
                status,
                content_type,
                steps: Mutex::new(Some(steps)),
                policies: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl HttpClient for FakeStreaming {
        async fn send(
            &self,
            _request: http::Request<Bytes>,
        ) -> Result<http::Response<Bytes>, HttpError> {
            Err(HttpError::Transport("buffered send used".to_owned()))
        }

        async fn send_streaming(
            &self,
            request: http::Request<Bytes>,
        ) -> Result<http::Response<ByteStream>, HttpError> {
            self.policies
                .lock()
                .expect("lock")
                .push(StreamPolicy::of_request(&request));
            let steps: VecDeque<Step> = self
                .steps
                .lock()
                .expect("lock")
                .take()
                .unwrap_or_default()
                .into();
            let body: ByteStream = Box::pin(futures_util::stream::unfold(
                steps,
                |mut steps| async move {
                    match steps.pop_front()? {
                        Step::Bytes(text) => Some((Ok(Bytes::from_static(text.as_bytes())), steps)),
                        Step::Fail => Some((
                            Err(HttpError::Transport("connection reset".to_owned())),
                            steps,
                        )),
                        Step::Stall => std::future::pending().await,
                    }
                },
            ));
            Ok(http::Response::builder()
                .status(self.status)
                .header(http::header::CONTENT_TYPE, self.content_type)
                .body(body)
                .expect("response"))
        }
    }

    /// Sends one streamed model call with `deadline` through the port's
    /// own bounds (`BoundedHttpClient`) over `fake`, timed by `clock`.
    fn run_capped(
        fake: Arc<FakeStreaming>,
        clock: &Arc<OnePoll>,
        wire: Wire,
        deadline: Duration,
        max_bytes: usize,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let http = BoundedHttpClient::new(fake, Arc::clone(clock) as Arc<dyn Clock>);
        let request = http::Request::builder()
            .extension(HttpPolicy {
                max_response_bytes: max_bytes,
                timeout: deadline,
            })
            .extension(policy(deadline))
            .body(Bytes::from_static(b"{\"stream\":true}"))
            .expect("request");
        pollster::block_on(receive(&http, request, wire, max_bytes))
    }

    fn run(
        fake: FakeStreaming,
        clock: &Arc<OnePoll>,
        wire: Wire,
        deadline: Duration,
    ) -> Result<http::Response<Bytes>, HttpError> {
        run_capped(Arc::new(fake), clock, wire, deadline, 1024 * 1024)
    }

    fn json(response: &http::Response<Bytes>) -> serde_json::Value {
        serde_json::from_slice(response.body()).expect("json")
    }

    const SSE: &str = "text/event-stream; charset=utf-8";

    #[test]
    fn the_policy_asks_for_the_idle_gap_and_a_capped_whole_deadline() {
        let asked = policy(Duration::from_secs(480));
        assert_eq!(asked.idle_timeout, IDLE_GAP);
        assert_eq!(asked.total_timeout, Duration::from_secs(480));
        assert_eq!(asked.max_bytes, MAX_STREAM_BYTES);
        assert_eq!(
            policy(Duration::from_secs(86_400)).total_timeout,
            MAX_STREAM_DEADLINE
        );
        // Both survive the port's clamp: a caller may only tighten.
        assert_eq!(asked.clamped(), asked);
        assert_eq!(
            policy(Duration::from_secs(86_400)).clamped().total_timeout,
            MAX_STREAM_DEADLINE
        );
    }

    #[test]
    fn a_thinking_stream_arrives_as_the_native_answer() {
        let fake = Arc::new(FakeStreaming::new(
            200,
            SSE,
            vec![
                Step::Bytes(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12000,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
                ),
                Step::Bytes(
                    "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_del",
                ),
                Step::Bytes(
                    "ta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"{}\"}}\r\n\r\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":900}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
            ],
        ));
        let clock = OnePoll::new(Duration::from_secs(1));
        let response = run_capped(
            Arc::clone(&fake),
            &clock,
            Wire::Anthropic,
            Duration::from_secs(600),
            1024 * 1024,
        )
        .expect("answered");
        assert_eq!(response.status(), 200);
        let body = json(&response);
        assert_eq!(body["content"][0]["text"], "{}");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 12000);
        assert_eq!(body["usage"]["output_tokens"], 900);
        // The transport was asked for the idle gap and the call's deadline.
        assert_eq!(
            *fake.policies.lock().expect("lock"),
            vec![policy(Duration::from_secs(600))]
        );
        // Every chunk waited at most the idle gap.
        let bounds = clock.bounds.lock().expect("lock").clone();
        assert!(bounds.iter().skip(1).all(|b| *b <= IDLE_GAP), "{bounds:?}");
    }

    #[test]
    fn a_stalled_stream_fails_on_the_idle_gap() {
        let fake = FakeStreaming::new(
            200,
            SSE,
            vec![
                Step::Bytes("data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n\n"),
                Step::Stall,
            ],
        );
        let clock = OnePoll::new(Duration::from_secs(1));
        let error = run(fake, &clock, Wire::Openai, Duration::from_secs(600)).expect_err("stalled");
        assert!(
            matches!(&error, HttpError::Transport(m) if m.contains("stalled") && m.contains("90s")),
            "{error:?}"
        );
    }

    #[test]
    fn a_stream_past_its_whole_deadline_fails_as_the_deadline() {
        let steps = (0..100)
            .map(|_| Step::Bytes(": ping\n\n"))
            .collect::<Vec<_>>();
        let fake = FakeStreaming::new(200, SSE, steps);
        // Each clock read is a minute: five minutes are gone by the fifth.
        let clock = OnePoll::new(Duration::from_secs(60));
        let error = run(fake, &clock, Wire::Openai, Duration::from_secs(300)).expect_err("late");
        assert!(
            matches!(error, HttpError::DeadlineExceeded { after } if after == Duration::from_secs(300)),
            "{error:?}"
        );
    }

    #[test]
    fn the_whole_deadline_is_capped() {
        let fake = FakeStreaming::new(200, SSE, vec![Step::Stall]);
        // A day asked for; the clock jumps by the cap between reads.
        let clock = OnePoll::new(MAX_STREAM_DEADLINE);
        let error =
            run(fake, &clock, Wire::Openai, Duration::from_secs(86_400)).expect_err("past the cap");
        assert!(
            matches!(error, HttpError::DeadlineExceeded { after } if after == MAX_STREAM_DEADLINE),
            "{error:?}"
        );
    }

    #[test]
    fn an_error_event_mid_stream_answers_its_status_and_body() {
        let fake = FakeStreaming::new(
            200,
            SSE,
            vec![
                Step::Bytes(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{}}\n\n",
                ),
                Step::Bytes(
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"slow down\"}}\n\n",
                ),
            ],
        );
        let clock = OnePoll::new(Duration::ZERO);
        let response =
            run(fake, &clock, Wire::Anthropic, Duration::from_secs(60)).expect("answered");
        assert_eq!(response.status(), 429);
        assert!(String::from_utf8_lossy(response.body()).contains("slow down"));
    }

    #[test]
    fn a_transport_failure_mid_stream_is_a_transport_error() {
        let fake = FakeStreaming::new(200, SSE, vec![Step::Bytes(": hi\n\n"), Step::Fail]);
        let clock = OnePoll::new(Duration::ZERO);
        let error = run(fake, &clock, Wire::Openai, Duration::from_secs(60)).expect_err("reset");
        assert!(matches!(error, HttpError::Transport(m) if m == "connection reset"));
    }

    #[test]
    fn a_refused_stream_answers_its_status_and_the_head_of_its_body() {
        let fake = FakeStreaming::new(524, "text/plain", vec![Step::Bytes("error code: 524")]);
        let clock = OnePoll::new(Duration::ZERO);
        let response =
            run(fake, &clock, Wire::Anthropic, Duration::from_secs(60)).expect("answered");
        assert_eq!(response.status(), 524);
        assert_eq!(response.body().as_ref(), b"error code: 524");
    }

    #[test]
    fn a_provider_that_ignores_stream_true_is_read_as_it_came() {
        let fake = FakeStreaming::new(
            200,
            "application/json",
            vec![
                Step::Bytes("{\"choices\":[{\"message\":"),
                Step::Bytes("{\"content\":\"ok\"}}]}"),
            ],
        );
        let clock = OnePoll::new(Duration::ZERO);
        let response = run(fake, &clock, Wire::Openai, Duration::from_secs(60)).expect("answered");
        assert_eq!(json(&response)["choices"][0]["message"]["content"], "ok");
        let big = FakeStreaming::new(200, "application/json", vec![Step::Bytes("0123456789")]);
        let error = run_capped(
            Arc::new(big),
            &clock,
            Wire::Openai,
            Duration::from_secs(60),
            4,
        )
        .expect_err("too large");
        assert!(matches!(error, HttpError::ResponseTooLarge { limit: 4 }));
    }
}
