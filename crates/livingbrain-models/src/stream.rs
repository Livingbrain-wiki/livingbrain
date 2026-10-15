//! The streaming seam: how a model call reads its reply as it arrives.
//!
//! `cratefield-core` 0.8 has no streaming port, and this crate may not name
//! a runtime. So the seam is declared here — [`StreamingHttp`] opens a
//! request and hands back its head and a [`ChunkStream`] of body bytes —
//! and the venture, the one crate allowed to name `worker`, implements it
//! over `worker::Fetch` and wires it into
//! [`crate::deadline::ModelDeadlineHttpClient::with_streaming`].
//!
//! Everything with a decision in it lives here, behind the trait, so it is
//! tested without a runtime: [`receive`] bounds the head, every gap between
//! chunks ([`IDLE_GAP`]) and the whole body ([`MAX_STREAM_DEADLINE`]) by the
//! `Clock` port, and reads the events with [`crate::sse::Accumulator`] as
//! they come, so a long thinking stream is never held in memory — only the
//! answer is.
//!
//! See [`crate::deadline`] for how this goes away on Cratefield 0.9.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{Clock, HttpError};
use time::OffsetDateTime;

use crate::catalog::Wire;
use crate::sse::{Accumulator, Failure};

/// The longest a stream may go without a byte. A thinking model streams
/// its reasoning token by token and providers send `ping`s, so a gap this
/// long is a stalled upstream, not a slow one. Under Cloudflare's 100 s
/// origin-idle cut, which the provider's own front would hit first anyway.
pub const IDLE_GAP: Duration = Duration::from_secs(90);

/// The longest a streamed model call may take in total, whatever its own
/// deadline asks. A cron invocation's wall is 15 minutes; a request route
/// runs while its client stays connected.
pub const MAX_STREAM_DEADLINE: Duration = Duration::from_secs(15 * 60);

/// How much of a refused stream's body is read for its error.
const MAX_ERROR_BYTES: usize = 4 * 1024;

/// Marks a model call that asked for `"stream": true`, and names the wire
/// its events are read as. A request extension, never on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelStream(pub(crate) Wire);

/// The body of an opened stream, a chunk at a time.
#[async_trait]
pub trait ChunkStream: Send {
    /// The next chunk; `None` once the body has ended.
    async fn next_chunk(&mut self) -> Option<Result<Bytes, HttpError>>;
}

/// An opened streaming response: its status and headers, and its body.
pub struct StreamHead {
    pub status: http::StatusCode,
    pub headers: http::HeaderMap,
    pub body: Box<dyn ChunkStream>,
}

/// Opens one request and answers as soon as the response head is in,
/// leaving the body to be read a chunk at a time. Implemented by the
/// venture over `worker::Fetch`; a failure message must never carry the
/// request URL or a header (the key is in one).
#[async_trait]
pub trait StreamingHttp: Send + Sync {
    async fn open(&self, request: http::Request<Bytes>) -> Result<StreamHead, HttpError>;
}

/// Sends a [`ModelStream`]-marked `request` through `streaming` and reads
/// it back into a buffered response [`crate::invoke::chat`] reads as it
/// reads any other:
///
/// - a non-2xx head: that status with the first [`MAX_ERROR_BYTES`] of its
///   body (the provider's error);
/// - a 2xx `text/event-stream`: the events read as they arrive into the
///   wire's non-streaming JSON shape — or, for an `error` event or a
///   stream that broke off, the status and body of a [`Failure`];
/// - any other 2xx (a provider that ignored `"stream": true`): the body
///   as it came, held to `max_bytes`.
///
/// The head and every gap between chunks are bounded by [`IDLE_GAP`], the
/// whole exchange by `deadline` capped at [`MAX_STREAM_DEADLINE`].
///
/// # Errors
///
/// [`HttpError::DeadlineExceeded`] past the whole deadline,
/// [`HttpError::Transport`] for an idle gap or a transport failure,
/// [`HttpError::ResponseTooLarge`] past `max_bytes`.
pub(crate) async fn receive(
    streaming: Arc<dyn StreamingHttp>,
    clock: &dyn Clock,
    request: http::Request<Bytes>,
    wire: Wire,
    deadline: Duration,
    max_bytes: usize,
) -> Result<http::Response<Bytes>, HttpError> {
    let total = deadline.min(MAX_STREAM_DEADLINE);
    let started = clock.now();
    let head = bounded(
        clock,
        async move { streaming.open(request).await },
        wait(clock, started, total)?,
        total,
    )
    .await??;
    let StreamHead {
        status,
        headers,
        mut body,
    } = head;
    let event_stream = headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));

    if !status.is_success() || !event_stream {
        let cap = if status.is_success() {
            max_bytes
        } else {
            MAX_ERROR_BYTES
        };
        let mut buffered = Vec::new();
        while let Some(chunk) = next(clock, &mut body, started, total).await? {
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
        return respond(status, Some(&headers), Bytes::from(buffered));
    }

    let mut accumulator = Accumulator::new(wire, max_bytes);
    let completion = loop {
        match next(clock, &mut body, started, total).await? {
            Some(chunk) => {
                if let Err(failure) = accumulator.feed(&chunk) {
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

/// The time left of `total` since `started`, or the deadline error.
fn wait(
    clock: &dyn Clock,
    started: OffsetDateTime,
    total: Duration,
) -> Result<Duration, HttpError> {
    let spent = clock.now() - started;
    let spent = Duration::try_from(spent).unwrap_or(Duration::ZERO);
    let left = total.saturating_sub(spent);
    if left.is_zero() {
        return Err(HttpError::DeadlineExceeded { after: total });
    }
    Ok(left.min(IDLE_GAP))
}

/// Runs `fut` under `after`: a lapse inside the whole deadline is an idle
/// gap, one that reaches it is the deadline.
async fn bounded<T: Send + 'static>(
    clock: &dyn Clock,
    fut: impl Future<Output = T> + Send + 'static,
    after: Duration,
    total: Duration,
) -> Result<T, HttpError> {
    match cratefield_core::timeout(clock, fut, after).await {
        Some(value) => Ok(value),
        None if after < IDLE_GAP => Err(HttpError::DeadlineExceeded { after: total }),
        None => Err(HttpError::Transport(format!(
            "the model stream stalled: no bytes for {}s",
            after.as_secs()
        ))),
    }
}

/// The next chunk under the idle and whole-body bounds.
async fn next(
    clock: &dyn Clock,
    body: &mut Box<dyn ChunkStream>,
    started: OffsetDateTime,
    total: Duration,
) -> Result<Option<Bytes>, HttpError> {
    let after = wait(clock, started, total)?;
    // `Clock::timeout` needs a `'static` future, so the stream moves into it
    // and comes back with its chunk; on a lapse it is dropped, which is the
    // end of the call anyway.
    let mut owned = std::mem::replace(body, Box::new(Ended));
    let (owned, chunk) = bounded(
        clock,
        async move {
            let chunk = owned.next_chunk().await;
            (owned, chunk)
        },
        after,
        total,
    )
    .await?;
    *body = owned;
    chunk.transpose()
}

/// The stand-in a body is swapped for while its next chunk is read.
struct Ended;

#[async_trait]
impl ChunkStream for Ended {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, HttpError>> {
        None
    }
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
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

    /// A clock whose timeout polls the future once: ready is an answer,
    /// pending is the bound lapsing. `now` advances by `step` per call.
    struct OnePoll {
        now: Mutex<OffsetDateTime>,
        step: Duration,
        bounds: Mutex<Vec<Duration>>,
    }

    impl OnePoll {
        fn new(step: Duration) -> Self {
            Self {
                now: Mutex::new(OffsetDateTime::UNIX_EPOCH),
                step,
                bounds: Mutex::new(Vec::new()),
            }
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

    struct Scripted(VecDeque<Step>);

    #[async_trait]
    impl ChunkStream for Scripted {
        async fn next_chunk(&mut self) -> Option<Result<Bytes, HttpError>> {
            match self.0.pop_front()? {
                Step::Bytes(text) => Some(Ok(Bytes::from_static(text.as_bytes()))),
                Step::Fail => Some(Err(HttpError::Transport("connection reset".to_owned()))),
                Step::Stall => std::future::pending().await,
            }
        }
    }

    struct FakeStreaming {
        status: u16,
        content_type: &'static str,
        steps: Mutex<Option<Vec<Step>>>,
        opened: Arc<Mutex<Vec<String>>>,
    }

    impl FakeStreaming {
        fn new(status: u16, content_type: &'static str, steps: Vec<Step>) -> Self {
            Self {
                status,
                content_type,
                steps: Mutex::new(Some(steps)),
                opened: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl StreamingHttp for FakeStreaming {
        async fn open(&self, request: http::Request<Bytes>) -> Result<StreamHead, HttpError> {
            self.opened
                .lock()
                .expect("lock")
                .push(String::from_utf8_lossy(request.body()).into_owned());
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static(self.content_type),
            );
            Ok(StreamHead {
                status: http::StatusCode::from_u16(self.status).expect("status"),
                headers,
                body: Box::new(Scripted(
                    self.steps
                        .lock()
                        .expect("lock")
                        .take()
                        .unwrap_or_default()
                        .into(),
                )),
            })
        }
    }

    fn run(
        fake: FakeStreaming,
        clock: &OnePoll,
        wire: Wire,
        deadline: Duration,
    ) -> Result<http::Response<Bytes>, HttpError> {
        pollster::block_on(receive(
            Arc::new(fake),
            clock,
            http::Request::new(Bytes::from_static(b"{\"stream\":true}")),
            wire,
            deadline,
            1024 * 1024,
        ))
    }

    fn json(response: &http::Response<Bytes>) -> serde_json::Value {
        serde_json::from_slice(response.body()).expect("json")
    }

    const SSE: &str = "text/event-stream; charset=utf-8";

    #[test]
    fn a_thinking_stream_arrives_as_the_native_answer() {
        let fake = FakeStreaming::new(
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
        );
        let clock = OnePoll::new(Duration::from_secs(1));
        let response =
            run(fake, &clock, Wire::Anthropic, Duration::from_secs(600)).expect("answered");
        assert_eq!(response.status(), 200);
        let body = json(&response);
        assert_eq!(body["content"][0]["text"], "{}");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 12000);
        assert_eq!(body["usage"]["output_tokens"], 900);
        // Head and every chunk waited at most the idle gap.
        assert!(
            clock
                .bounds
                .lock()
                .expect("lock")
                .iter()
                .all(|b| *b <= IDLE_GAP)
        );
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
        let clock = OnePoll::new(Duration::ZERO);
        let error =
            run(fake, &clock, Wire::Openai, Duration::from_secs(86_400)).expect_err("stalled");
        assert!(matches!(error, HttpError::Transport(_)), "{error:?}");
        let error = pollster::block_on(receive(
            Arc::new(FakeStreaming::new(200, SSE, vec![])),
            &OnePoll::new(MAX_STREAM_DEADLINE),
            http::Request::new(Bytes::new()),
            Wire::Openai,
            Duration::from_secs(86_400),
            1024,
        ))
        .expect_err("past the cap");
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
        let error = pollster::block_on(receive(
            Arc::new(big),
            &clock,
            http::Request::new(Bytes::new()),
            Wire::Openai,
            Duration::from_secs(60),
            4,
        ))
        .expect_err("too large");
        assert!(matches!(error, HttpError::ResponseTooLarge { limit: 4 }));
    }
}
