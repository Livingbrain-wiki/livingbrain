//! A model call's own deadline, past the `HttpClient` port's 30 s ceiling.
//!
//! **Why this exists.** `cratefield-core` 0.8.0 clamps every
//! [`HttpPolicy::timeout`] to `MAX_RESPONSE_TIMEOUT` (30 s,
//! `src/ports/http.rs`), and the Workers runtime enforces the clamped value
//! over the *whole* exchange — head and fully buffered body — by wrapping
//! its `FetchClient` in a `BoundedHttpClient` raced against `worker::Delay`
//! (`cratefield-runtime-cloudflare` 0.4.0, `src/runtime.rs`). A
//! forced-thinking compile on GLM-5.3 takes 55–100 s, so every one failed
//! as `model-http: deadline exceeded after 30s` however long the caller
//! asked for. Streaming the reply would not help on this port: the deadline
//! covers the buffered body too.
//!
//! **What it does.** A model call attaches [`ModelDeadline`] beside its
//! [`HttpPolicy`] (see [`policy`]). [`ModelDeadlineHttpClient`] — which the
//! venture wires as `ports.http` wherever it derives the ports itself (the
//! CLI routes and the cron tick) — honours that deadline up to
//! [`MAX_MODEL_DEADLINE`], and treats every other request exactly as the
//! runtime's `BoundedHttpClient` does: the clamped [`HttpPolicy`], deadline
//! and size cap alike. A request without the marker cannot get a longer
//! deadline through it, and on a path whose ports the runtime builds the
//! marker is simply ignored (the call stays at 30 s).
//!
//! **When it goes.** Cratefield's streaming port (`send_streaming`, harness
//! issue #859, unreleased at `cratefield-core` 0.8.4) bounds a streamed
//! body by an idle gap and a whole-body ceiling instead of 30 s. Once the
//! pins move to it, a model call can stream (`"stream": true`) and this
//! wrapper can be deleted.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{Clock, HttpClient, HttpError, HttpPolicy, declared_content_length};

/// The longest deadline a [`ModelDeadline`] may ask for. Past the slowest
/// measured compile (≈100 s) with room, and short enough that one call
/// cannot eat a cron invocation's 15-minute wall: the compiler checks its
/// 5-minute budget before each job, so a tick overruns by one call at most.
pub const MAX_MODEL_DEADLINE: Duration = Duration::from_secs(300);

/// Marks an outbound request as a model call and carries its deadline,
/// which [`ModelDeadlineHttpClient`] honours up to [`MAX_MODEL_DEADLINE`]
/// instead of the port's 30 s ceiling. Like [`HttpPolicy`] it is a request
/// extension and never reaches the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelDeadline(pub Duration);

/// The extensions a model call attaches: the port's [`HttpPolicy`] (which
/// every other `HttpClient` honours, clamped) and the [`ModelDeadline`]
/// that lifts the clamp where the venture's client is wired.
pub fn policy(
    builder: http::request::Builder,
    max_response_bytes: usize,
    timeout: Duration,
) -> http::request::Builder {
    builder
        .extension(HttpPolicy {
            max_response_bytes,
            timeout,
        })
        .extension(ModelDeadline(timeout))
}

/// The deadline [`ModelDeadlineHttpClient`] applies to `request`: its
/// [`ModelDeadline`] capped at [`MAX_MODEL_DEADLINE`], else the port's own
/// clamped [`HttpPolicy::timeout`].
#[must_use]
pub fn effective_timeout<T>(request: &http::Request<T>) -> Duration {
    request.extensions().get::<ModelDeadline>().map_or_else(
        || HttpPolicy::of_request(request).timeout,
        |ModelDeadline(timeout)| (*timeout).min(MAX_MODEL_DEADLINE),
    )
}

/// The port's bounds with a model call's deadline honoured: the
/// venture-side stand-in for `BoundedHttpClient` (see the module docs).
pub struct ModelDeadlineHttpClient {
    inner: Arc<dyn HttpClient>,
    clock: Arc<dyn Clock>,
}

impl ModelDeadlineHttpClient {
    /// Bounds `inner`'s sends by `clock`-measured deadlines and the
    /// [`HttpPolicy`] size cap.
    #[must_use]
    pub fn new(inner: Arc<dyn HttpClient>, clock: Arc<dyn Clock>) -> Self {
        Self { inner, clock }
    }
}

#[async_trait]
impl HttpClient for ModelDeadlineHttpClient {
    async fn send(
        &self,
        request: http::Request<Bytes>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let limit = HttpPolicy::of_request(&request).max_response_bytes;
        let after = effective_timeout(&request);
        let inner = Arc::clone(&self.inner);
        let Some(outcome) = cratefield_core::timeout(
            self.clock.as_ref(),
            async move { inner.send(request).await },
            after,
        )
        .await
        else {
            return Err(HttpError::DeadlineExceeded { after });
        };
        let response = outcome?;
        if declared_content_length(response.headers()).is_some_and(|len| len > limit)
            || response.body().len() > limit
        {
            return Err(HttpError::ResponseTooLarge { limit });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_core::{DEFAULT_RESPONSE_TIMEOUT, MAX_RESPONSE_TIMEOUT};
    use std::any::Any;
    use std::pin::Pin;

    type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
    use std::sync::Mutex;

    /// Records the deadline each call was raced against, and expires the
    /// call when that deadline is at or under `expire_at_or_below`.
    struct RecordingClock {
        seen: Mutex<Vec<Duration>>,
        expire_at_or_below: Duration,
    }

    impl RecordingClock {
        fn new(expire_at_or_below: Duration) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::new(Vec::new()),
                expire_at_or_below,
            })
        }

        fn seen(&self) -> Vec<Duration> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl Clock for RecordingClock {
        fn now(&self) -> time::OffsetDateTime {
            time::OffsetDateTime::UNIX_EPOCH
        }

        async fn timeout_any(
            &self,
            fut: BoxFuture<Box<dyn Any + Send>>,
            after: Duration,
        ) -> Option<Box<dyn Any + Send>> {
            self.seen.lock().expect("lock").push(after);
            if after <= self.expire_at_or_below {
                None
            } else {
                Some(fut.await)
            }
        }
    }

    /// Answers every request with `body`.
    struct Answer(&'static str);

    #[async_trait]
    impl HttpClient for Answer {
        async fn send(
            &self,
            _request: http::Request<Bytes>,
        ) -> Result<http::Response<Bytes>, HttpError> {
            Ok(http::Response::new(Bytes::from_static(self.0.as_bytes())))
        }
    }

    fn client(clock: &Arc<RecordingClock>, body: &'static str) -> ModelDeadlineHttpClient {
        ModelDeadlineHttpClient::new(Arc::new(Answer(body)), Arc::clone(clock) as Arc<dyn Clock>)
    }

    fn model_call(timeout: Duration) -> http::Request<Bytes> {
        policy(
            http::Request::builder().uri("https://api.z.ai/"),
            1024,
            timeout,
        )
        .body(Bytes::new())
        .expect("request")
    }

    #[test]
    fn a_model_call_gets_its_own_deadline_past_the_port_ceiling() {
        let clock = RecordingClock::new(Duration::ZERO);
        let response =
            pollster::block_on(client(&clock, "ok").send(model_call(Duration::from_secs(300))))
                .expect("answered");
        assert_eq!(response.body().as_ref(), b"ok");
        assert_eq!(clock.seen(), vec![Duration::from_secs(300)]);
        assert!(Duration::from_secs(300) > MAX_RESPONSE_TIMEOUT);
    }

    #[test]
    fn a_model_deadline_is_capped() {
        let clock = RecordingClock::new(Duration::ZERO);
        pollster::block_on(client(&clock, "ok").send(model_call(Duration::from_secs(3600))))
            .expect("answered");
        assert_eq!(clock.seen(), vec![MAX_MODEL_DEADLINE]);
    }

    #[test]
    fn an_unmarked_request_keeps_the_port_clamp() {
        let clock = RecordingClock::new(Duration::ZERO);
        let long = http::Request::builder()
            .extension(HttpPolicy {
                max_response_bytes: 1024,
                timeout: Duration::from_secs(300),
            })
            .body(Bytes::new())
            .expect("request");
        pollster::block_on(client(&clock, "ok").send(long)).expect("answered");
        let bare = http::Request::new(Bytes::new());
        pollster::block_on(client(&clock, "ok").send(bare)).expect("answered");
        assert_eq!(
            clock.seen(),
            vec![MAX_RESPONSE_TIMEOUT, DEFAULT_RESPONSE_TIMEOUT]
        );
    }

    #[test]
    fn an_expired_call_names_the_deadline_it_ran_out_of() {
        let clock = RecordingClock::new(Duration::from_secs(600));
        let error =
            pollster::block_on(client(&clock, "ok").send(model_call(Duration::from_secs(120))))
                .expect_err("expired");
        assert!(
            matches!(error, HttpError::DeadlineExceeded { after } if after == Duration::from_secs(120)),
            "{error:?}"
        );
    }

    #[test]
    fn the_size_cap_still_holds() {
        let clock = RecordingClock::new(Duration::ZERO);
        let request = policy(http::Request::builder(), 4, Duration::from_secs(60))
            .body(Bytes::new())
            .expect("request");
        let error =
            pollster::block_on(client(&clock, "too long").send(request)).expect_err("over the cap");
        assert!(
            matches!(error, HttpError::ResponseTooLarge { limit: 4 }),
            "{error:?}"
        );
    }
}
