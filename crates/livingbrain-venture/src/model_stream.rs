//! The streaming seam (`livingbrain_models::stream::StreamingHttp`) over
//! `worker::Fetch`: a model call that asked for `"stream": true` is opened
//! here and its body handed back as it arrives, a chunk at a time, instead
//! of being buffered whole by the runtime's `FetchClient`.
//!
//! Only the transport lives here. The bounds (the head, the idle gap between
//! chunks, the whole-call ceiling) and the reading of the events are
//! `livingbrain_models::stream::receive`'s, tested there over a fake stream.
//!
//! The request goes out exactly as `FetchClient` sends it: the headers as
//! given (the key travels only in its header), the JSON body as text. And,
//! as there, no failure is ever stringified whole — workerd names the URL
//! it could not reach, and a cause may quote a header — so every error goes
//! through [`transport`]: the URL cut to its origin, then scrubbed.
//!
//! Goes away with `cratefield-core` 0.9's `HttpClient::send_streaming` (see
//! `livingbrain_models::deadline`).

use std::fmt::Write as _;
use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use cratefield_core::axum::http;
use cratefield_core::{HttpError, scrub_request_url, scrub_text};
use futures_util::StreamExt;
use livingbrain_models::stream::{ChunkStream, StreamHead, StreamingHttp};
use worker::send::{IntoSendFuture, SendFuture, SendWrapper};
use worker::{ByteStream, Fetch, Headers, Method, Request as WorkerRequest, RequestInit};

/// Opens a model call over the isolate's own `fetch` and streams its body.
pub struct FetchStreaming;

#[async_trait]
impl StreamingHttp for FetchStreaming {
    async fn open(&self, request: http::Request<Bytes>) -> Result<StreamHead, HttpError> {
        let (parts, body) = request.into_parts();
        let url = parts.uri.to_string();
        let mut init = RequestInit::new();
        init.method = Method::from(parts.method.as_str().to_string());
        let headers = Headers::new();
        for (name, value) in &parts.headers {
            let _ = headers.set(name.as_str(), value.to_str().unwrap_or_default());
        }
        init.headers = headers;
        if !body.is_empty() {
            let text = String::from_utf8(body.to_vec()).map_err(|err| transport(&err, &url))?;
            init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(&text)));
        }
        let outgoing =
            WorkerRequest::new_with_init(&url, &init).map_err(|err| transport(&err, &url))?;
        let mut response = Fetch::Request(outgoing)
            .send()
            .into_send()
            .await
            .map_err(|err| transport(&err, &url))?;

        let status = http::StatusCode::from_u16(response.status_code())
            .map_err(|err| transport(&err, &url))?;
        let mut head = http::HeaderMap::new();
        for (name, value) in response.headers() {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::try_from(name.as_str()),
                http::HeaderValue::try_from(value.as_str()),
            ) {
                head.append(name, value);
            }
        }
        let stream = response.stream().map_err(|err| transport(&err, &url))?;
        Ok(StreamHead {
            status,
            headers: head,
            body: Box::new(WorkerChunks {
                stream: SendWrapper::new(Box::pin(stream)),
                url,
            }),
        })
    }
}

/// A fetch response's body, a chunk at a time. `ByteStream` holds JS
/// handles (single-threaded isolate); `SendWrapper` / `SendFuture` are the
/// workers-rs-sanctioned `Send` claims, as the runtime's ports use them.
struct WorkerChunks {
    stream: SendWrapper<Pin<Box<ByteStream>>>,
    /// Kept only to scrub a failure against.
    url: String,
}

#[async_trait]
impl ChunkStream for WorkerChunks {
    async fn next_chunk(&mut self) -> Option<Result<Bytes, HttpError>> {
        let item = SendFuture::new(self.stream.next()).await?;
        Some(
            item.map(Bytes::from)
                .map_err(|err| transport(&err, &self.url)),
        )
    }
}

/// An error as a message safe to hand on: the cause chain walked (the
/// `Display` of a wrapping `worker::Error` is nearly empty), the request URL
/// cut to its origin, everything else scrubbed — the runtime's
/// `FetchClient` discipline (`cratefield-runtime-cloudflare`, issue #229).
fn transport(err: &dyn std::error::Error, url: &str) -> HttpError {
    let mut message = err.to_string();
    let mut cause = err.source();
    while let Some(current) = cause {
        let _ = write!(message, ": {current}");
        cause = current.source();
    }
    HttpError::Transport(scrub_text(&scrub_request_url(&message, url)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_names_the_origin_and_never_the_path_or_a_key() {
        let url = "https://api.z.ai/api/anthropic/v1/messages?key=sk-live-SECRET123456";
        let err = worker::Error::JsError(format!(
            "TypeError: Fetch API cannot load: {url}. Authorization: Bearer sk-live-SECRET123456"
        ));
        let HttpError::Transport(message) = transport(&err, url) else {
            panic!("a transport failure");
        };
        assert!(message.contains("https://api.z.ai"), "{message}");
        assert!(!message.contains("SECRET123456"), "{message}");
        assert!(!message.contains("/v1/messages"), "{message}");
    }
}
