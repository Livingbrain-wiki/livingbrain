//! The HTTP client for the Living Brain API, and the wire contract it speaks.
//!
//! The CLI holds no state: no cache, no memory, no agent loop; the server does
//! everything else. Every request carries `User-Agent: livingbrain-cli/<ver>`,
//! and every request but the two auth endpoints carries
//! `Authorization: Bearer <access_token>`.
//!
//! | Request | Body | Success response |
//! | :--- | :--- | :--- |
//! | `POST /v1/device-auth/code` | `{"client_id":"livingbrain-cli"}` | `{device_code, user_code, verification_uri, verification_uri_complete?, interval, expires_in}` |
//! | `POST /v1/device-auth/token` | form `grant_type=urn:ietf:params:oauth:grant-type:device_code&device_code=…&client_id=livingbrain-cli` | `{access_token}` |
//! | `POST /v1/ask` | `{question, project?, as_of?}` | `{answer, citations:[{title, url, quote?}]}` |
//! | `GET /v1/search` | `?q=&project=&limit=` | `{results:[{title, url, snippet}]}` |
//! | `POST /v1/notes` | `{body, project?, title?, slug?}` | `{id, url, version, redacted, unchanged}` |
//! | `GET /v1/pages/{slug}` | — (slug percent-encoded) | `{slug, title, markdown, url}` |
//! | `GET /v1/export` | `?format=obsidian` | a zip response body |
//! | `POST /v1/pages/sources` | `{kind:"import"\|"agent_log", path, body, scope, project?}` | `{id, kind, scope, path, project, sha256, wikilinks:[], redacted, created, created_at}` (issues #81, #45) |
//! | `GET /v1/sources` | `?kind=&project=&limit=&cursor=` | `{sources:[{id, kind, scope, path, project, held, created_at}], next_cursor}` |
//! | `GET /v1/sources/projects` | `?kind=` | `{projects:[{project, sources}]}` |
//! | `GET /v1/model-jobs/next` | — | `{job:{id, kind, agent, model, settings, system, user, max_tokens, json, lease_until}}`, or `204` with no job (the local runner, `jobs run`) |
//! | `POST /v1/model-jobs/{id}/result` | `{text, prompt_tokens, completion_tokens}` | `{status:"done"}`; `422` when the server's validation refuses the text |
//! | `POST /v1/model-jobs/{id}/fail` | `{reason}` | `{status:"failed"}` |
//!
//! `POST /v1/pages/sources` is the one request that retries: the server dedupes
//! on `(scope, body_sha256)`, so a resend cannot create a second source. A
//! transient failure (a reset, refusal, close or timeout, or a `429`, `502`,
//! `503` or `504`) gets [`UPLOAD_ATTEMPTS`] tries in all; a `4xx` never does.
//!
//! `POST /v1/device-auth/token` answers `400 {error}` with one of
//! `authorization_pending`, `slow_down`, `expired_token`, `access_denied`
//! (RFC 8628 §3.5). Any other non-2xx is read as `{error}` or `{message}`, or as
//! the RFC 9457 problem document the modules answer with (`title`, `detail`).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::io::ErrorKind;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::auth::Token;
use crate::{CliError, CliResult, err};

/// How long `ask` waits for a member's own runner to answer before it gives
/// up and says which runner it was waiting for.
const ASK_RUNNER_WAIT: Duration = Duration::from_secs(300);

/// A response body, already read.
type Response = ureq::http::Response<ureq::Body>;

/// The tries a source upload gets: the first, and two retries.
const UPLOAD_ATTEMPTS: u32 = 3;

/// The pause before each retry, before jitter. Three tries leave two pauses;
/// the schedule's third step (about 15 seconds) would only be used by a fourth.
const RETRY_PAUSES: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(4)];

/// The longest a `Retry-After` header may hold a retry back, in seconds.
const RETRY_AFTER_CAP: u64 = 30;

/// The statuses that name a passing fault rather than a bad request.
const RETRY_STATUSES: [u16; 4] = [429, 502, 503, 504];

/// What one poll of the token endpoint ended as (RFC 8628 §3.5).
pub enum Poll {
    /// The human approved: here is the access token.
    Issued(String),
    /// `authorization_pending` — keep waiting.
    Pending,
    /// `slow_down` — add five seconds to every later poll.
    SlowDown,
}

impl Poll {
    fn from_body(body: &[u8]) -> CliResult<Self> {
        let value: Value = serde_json::from_slice(body)
            .map_err(|e| err(format!("the token response was not JSON: {e}")))?;
        if let Some(token) = value.get("access_token").and_then(Value::as_str) {
            return Ok(Self::Issued(token.to_owned()));
        }
        match value.get("error").and_then(Value::as_str).unwrap_or("") {
            "authorization_pending" => Ok(Self::Pending),
            "slow_down" => Ok(Self::SlowDown),
            "expired_token" => Err(err("the device code expired before it was approved")),
            "access_denied" => Err(err("the authorization was denied")),
            other => Err(CliError::Api(format!(
                "the server refused the token poll: {other}"
            ))),
        }
    }
}

/// A client for one API base URL: tokenless for `login`, bearer for the rest.
pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: Option<Token>,
}

impl Client {
    /// A tokenless client (the two auth endpoints only need this).
    #[must_use]
    pub fn new(base_url: &str) -> Self {
        let config = ureq::Agent::config_builder()
            .user_agent(concat!("livingbrain-cli/", env!("CARGO_PKG_VERSION")))
            .timeout_global(Some(Duration::from_secs(30)))
            // Read a 4xx/5xx body so the server's own message can be surfaced.
            .http_status_as_error(false)
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
            base: base_url.trim_end_matches('/').to_owned(),
            token: None,
        }
    }

    /// Attach the bearer token every non-auth request uses.
    #[must_use]
    pub fn with_token(mut self, token: Token) -> Self {
        self.token = Some(token);
        self
    }

    fn get(&self, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithoutBody> {
        let request = self.agent.get(format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {}", token.expose())),
            None => request,
        }
    }

    fn post(&self, path: &str) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
        let request = self.agent.post(format!("{}{path}", self.base));
        match &self.token {
            Some(token) => request.header("Authorization", format!("Bearer {}", token.expose())),
            None => request,
        }
    }

    /// `POST /v1/device-auth/code` — the device authorization request (RFC 8628 §3.1).
    pub fn device_authorize(&self) -> CliResult<Value> {
        let response = self
            .post("/v1/device-auth/code")
            .send_json(json!({ "client_id": "livingbrain-cli" }))
            .map_err(transport)?;
        read_json(response)
    }

    /// `POST /v1/device-auth/token` — one poll of the device token endpoint.
    pub fn device_token(&self, device_code: &str) -> CliResult<Poll> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            // A device code is issued for one client, so the poll names it.
            ("client_id", "livingbrain-cli"),
        ];
        let response = self
            .post("/v1/device-auth/token")
            .send_form(form)
            .map_err(transport)?;
        let status = response.status().as_u16();
        let body = response.into_body().read_to_vec().map_err(transport)?;
        // A granted token (2xx) and the RFC 8628 refusals (400) both carry the
        // JSON `from_body` reads; anything else is a plain API error.
        if (200..300).contains(&status) || status == 400 {
            Poll::from_body(&body)
        } else {
            Err(api_error(status, &body))
        }
    }

    /// `POST /v1/ask`. `as_of` asks for the answer as of that partial date
    /// (docs/temporal-facts.md §5); the server validates it.
    ///
    /// An asker who runs their jobs on their own device gets `202 {"status":
    /// "pending", "poll", "retry_after"}`: their runner (`livingbrain jobs
    /// run`, or the Mac app) makes the call. This polls the job until it
    /// ends — `on_wait` runs once, so the caller can say what it waits for —
    /// and asks again, which reads the runner's answer. After
    /// [`ASK_RUNNER_WAIT`] it gives up with an error naming the runner.
    pub fn ask(
        &self,
        question: &str,
        project: Option<&str>,
        as_of: Option<&str>,
        on_wait: impl FnOnce(),
    ) -> CliResult<Value> {
        let mut body = json!({ "question": question });
        if let Some(as_of) = as_of {
            body["as_of"] = Value::String(as_of.to_owned());
        }
        let first = self.post_json("/v1/ask", body.clone(), project)?;
        if first.get("status").and_then(Value::as_str) != Some("pending") {
            return Ok(first);
        }
        on_wait();
        let poll = first
            .get("poll")
            .and_then(Value::as_str)
            .filter(|path| path.starts_with("/v1/model-jobs/"))
            .ok_or_else(|| CliError::Api("the server named no job to wait for".to_owned()))?
            .to_owned();
        let pause = Duration::from_secs(
            first
                .get("retry_after")
                .and_then(Value::as_u64)
                .unwrap_or(2)
                .clamp(1, 30),
        );
        let started = Instant::now();
        loop {
            if started.elapsed() > ASK_RUNNER_WAIT {
                return Err(CliError::Api(
                    "your runner did not answer in time — is `livingbrain jobs run` (or the \
                     Mac app's \"Run brain jobs with my subscription\") running?"
                        .to_owned(),
                ));
            }
            std::thread::sleep(pause);
            let (status, raw) = self.get_status(&poll)?;
            if status == 401 {
                return Err(CliError::NotLoggedIn(
                    "not logged in — run `livingbrain login`".to_owned(),
                ));
            }
            let state = serde_json::from_slice::<Value>(&raw)
                .ok()
                .and_then(|job| job.get("status").and_then(Value::as_str).map(str::to_owned));
            match state.as_deref() {
                Some("queued" | "claimed") => {}
                // Done or failed (the ask reads either), or gone: ask again.
                _ => return self.post_json("/v1/ask", body, project),
            }
        }
    }

    /// `GET /v1/search`.
    pub fn search(
        &self,
        query: &str,
        project: Option<&str>,
        limit: Option<u32>,
    ) -> CliResult<Value> {
        let mut request = self.get("/v1/search").query("q", query);
        if let Some(project) = project {
            request = request.query("project", project);
        }
        if let Some(limit) = limit {
            request = request.query("limit", limit.to_string());
        }
        read_json(request.call().map_err(transport)?)
    }

    /// `POST /v1/notes`.
    pub fn note(&self, body_text: &str, project: Option<&str>) -> CliResult<Value> {
        self.post_json("/v1/notes", json!({ "body": body_text }), project)
    }

    /// `POST /v1/notes` with the page named by the caller: `slug` keys the
    /// note, so the same slug updates one page in place, and a resend whose
    /// markdown is already the page's answers `unchanged: true` without a
    /// new version. `livingbrain logs sync` keeps one note per session this
    /// way.
    pub fn keyed_note(
        &self,
        slug: &str,
        title: &str,
        body_text: &str,
        project: Option<&str>,
    ) -> CliResult<Value> {
        self.post_json(
            "/v1/notes",
            json!({ "body": body_text, "title": title, "slug": slug }),
            project,
        )
    }

    /// `GET /v1/pages/{slug}`.
    pub fn page(&self, slug: &str) -> CliResult<Value> {
        let path = format!("/v1/pages/{}", encode_segment(slug));
        read_json(self.get(&path).call().map_err(transport)?)
    }

    /// `GET /v1/export` — the zip bytes.
    pub fn export(&self, format: &str) -> CliResult<Vec<u8>> {
        let response = self
            .get("/v1/export")
            .query("format", format)
            .call()
            .map_err(transport)?;
        read_bytes(response)
    }

    /// `POST /v1/pages/sources` — add one source: an imported file (`kind`
    /// `import`) or an agent session's normalised log (`kind` `agent_log`).
    ///
    /// For `import`, `path` is the vault-relative path with `/` separators;
    /// for `agent_log`, it is the session's filing name, `<agent> session
    /// <id>`. `body` is the already-redacted text; the server re-redacts and
    /// derives the wikilinks itself, and answers `201` for a new source or
    /// `200` for one it already holds (the idempotency key is the scope plus
    /// the body's SHA-256). `project`, when given, files the source under a
    /// project (`logs sync` sends the session's repository basename); a
    /// re-upload that names one fills in a row that had none.
    ///
    /// A transient failure is retried ([`UPLOAD_ATTEMPTS`] tries in all); the
    /// error that comes back is the last try's.
    pub fn post_source(
        &self,
        kind: &str,
        path: &str,
        body: &str,
        scope: &str,
        project: Option<&str>,
        origin_ref: Option<&str>,
    ) -> CliResult<Value> {
        let mut envelope = json!({ "kind": kind, "path": path, "body": body, "scope": scope });
        if let Some(origin_ref) = origin_ref {
            envelope["origin_ref"] = Value::String(origin_ref.to_owned());
        }
        let envelope = with_project(envelope, project);
        let response = retrying(
            || self.send_attempt(self.post("/v1/pages/sources").send_json(&envelope)),
            |tried, after| std::thread::sleep(retry_pause(tried, after)),
        )?;
        read_json(response)
    }

    /// Judge one try at a request: a retryable status or a transient transport
    /// failure is [`Attempt::Transient`], anything else is final.
    fn send_attempt(&self, sent: Result<Response, ureq::Error>) -> Attempt<Response> {
        match sent {
            Ok(response) if RETRY_STATUSES.contains(&response.status().as_u16()) => {
                let status = response.status().as_u16();
                let after = retry_after(&response);
                match response.into_body().read_to_vec() {
                    Ok(body) => Attempt::Transient {
                        error: api_error(status, &body),
                        after,
                    },
                    Err(error) => Attempt::Transient {
                        error: transport(error),
                        after,
                    },
                }
            }
            Ok(response) => Attempt::Done(response),
            Err(error) if is_transient(&error) => Attempt::Transient {
                error: transport(error),
                after: None,
            },
            Err(error) => Attempt::Fatal(transport(error)),
        }
    }

    /// `GET /v1/me`: who the token speaks for, including its workspace id. Used
    /// to confirm the default token really belongs to the default workspace.
    pub fn me(&self) -> CliResult<Value> {
        read_json(self.get("/v1/me").call().map_err(transport)?)
    }

    /// `GET /v1/sources` — the caller's sources, newest first, without their
    /// bodies; `cursor` is the previous page's `next_cursor`.
    pub fn sources(
        &self,
        kind: Option<&str>,
        project: Option<&str>,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> CliResult<Value> {
        let mut request = self.get("/v1/sources");
        for (name, value) in [("kind", kind), ("project", project), ("cursor", cursor)] {
            if let Some(value) = value {
                request = request.query(name, value);
            }
        }
        if let Some(limit) = limit {
            request = request.query("limit", limit.to_string());
        }
        read_json(request.call().map_err(transport)?)
    }

    /// `GET /v1/sources/projects` — the projects the caller's sources are
    /// filed under, with a count each.
    pub fn source_projects(&self, kind: Option<&str>) -> CliResult<Value> {
        let mut request = self.get("/v1/sources/projects");
        if let Some(kind) = kind {
            request = request.query("kind", kind);
        }
        read_json(request.call().map_err(transport)?)
    }

    /// `GET <path>`, answered as its status and body without reading a
    /// non-2xx as an error: the local runner (`jobs run`) tells a `204` (no
    /// job), a `401` (signed out) and a `404` (a job that is gone) apart
    /// itself. Only a transport failure is an `Err`.
    pub fn get_status(&self, path: &str) -> CliResult<(u16, Vec<u8>)> {
        let response = self.get(path).call().map_err(transport)?;
        let status = response.status().as_u16();
        let body = response.into_body().read_to_vec().map_err(transport)?;
        Ok((status, body))
    }

    /// `POST <path>` with a JSON body, answered as in [`Client::get_status`].
    pub fn post_status(&self, path: &str, body: &Value) -> CliResult<(u16, Vec<u8>)> {
        let response = self.post(path).send_json(body).map_err(transport)?;
        let status = response.status().as_u16();
        let body = response.into_body().read_to_vec().map_err(transport)?;
        Ok((status, body))
    }

    /// POST one JSON body (plus `project`) and read the JSON answer.
    fn post_json(&self, path: &str, body: Value, project: Option<&str>) -> CliResult<Value> {
        let body = with_project(body, project);
        read_json(self.post(path).send_json(&body).map_err(transport)?)
    }
}

/// `body` with `project` set, when there is one.
fn with_project(mut body: Value, project: Option<&str>) -> Value {
    if let Some(project) = project {
        body["project"] = Value::String(project.to_owned());
    }
    body
}

/// What one try at a request came to, as [`retrying`] judges it.
enum Attempt<T> {
    Done(T),
    /// Worth another try. `after` is the server's `Retry-After`, in seconds.
    Transient {
        error: CliError,
        after: Option<u64>,
    },
    /// Final: a `4xx`, a refusal, anything a retry would only repeat.
    Fatal(CliError),
}

/// Run `attempt` until it is done, fails for good, or runs out of tries. Before
/// each retry `pause` is called with the number of the try that just failed and
/// the server's hint, and it waits.
fn retrying<T>(
    mut attempt: impl FnMut() -> Attempt<T>,
    mut pause: impl FnMut(u32, Option<u64>),
) -> CliResult<T> {
    let mut tried = 1;
    loop {
        match attempt() {
            Attempt::Done(value) => return Ok(value),
            Attempt::Fatal(error) => return Err(error),
            Attempt::Transient { error, .. } if tried >= UPLOAD_ATTEMPTS => return Err(error),
            Attempt::Transient { after, .. } => pause(tried, after),
        }
        tried += 1;
    }
}

/// Whether a transport failure is worth another try: the connection was reset,
/// refused, closed or timed out. A name that will not resolve is not, because
/// a retry would only repeat it.
fn is_transient(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Timeout(_) | ureq::Error::ConnectionFailed => true,
        ureq::Error::Io(error) => matches!(
            error.kind(),
            ErrorKind::ConnectionReset
                | ErrorKind::ConnectionRefused
                | ErrorKind::ConnectionAborted
                | ErrorKind::BrokenPipe
                | ErrorKind::UnexpectedEof
                | ErrorKind::TimedOut
        ),
        _ => false,
    }
}

/// The `Retry-After` header as whole seconds, when it is one.
fn retry_after(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(ureq::http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The pause before retry number `tried`: the server's `Retry-After` when it
/// gave one, the schedule otherwise, jittered either way so a fleet of runs
/// does not retry in step. A debug-only override (`LIVINGBRAIN_TEST_RETRY_PAUSE`,
/// milliseconds) lets tests retry without sleeping; it does not exist in a
/// release build.
fn retry_pause(tried: u32, after: Option<u64>) -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(millis) = std::env::var("LIVINGBRAIN_TEST_RETRY_PAUSE") {
        return Duration::from_millis(millis.parse().unwrap_or(0));
    }
    let base = match after {
        Some(secs) => Duration::from_secs(secs.clamp(1, RETRY_AFTER_CAP)),
        None => RETRY_PAUSES[(tried as usize - 1).min(RETRY_PAUSES.len() - 1)],
    };
    jittered(base)
}

/// `base` scaled by a factor in [0.75, 1.25). std's `RandomState` is the CLI's
/// one source of randomness (see `telemetry.rs`), and a jitter needs no more.
fn jittered(base: Duration) -> Duration {
    let unit = (RandomState::new().build_hasher().finish() % 1000) as f64 / 1000.0;
    base.mul_f64(0.75 + 0.5 * unit)
}

/// Percent-encode one path segment (RFC 3986 unreserved characters kept).
pub(crate) fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// Map a transport failure to a CLI error.
fn transport(error: ureq::Error) -> CliError {
    CliError::Api(format!("could not reach the API: {error}"))
}

/// Read a response, turning a non-2xx into the server's message.
fn read_bytes(response: Response) -> CliResult<Vec<u8>> {
    let status = response.status().as_u16();
    let body = response.into_body().read_to_vec().map_err(transport)?;
    if (200..300).contains(&status) {
        Ok(body)
    } else {
        Err(api_error(status, &body))
    }
}

/// Read a response as JSON.
fn read_json(response: Response) -> CliResult<Value> {
    serde_json::from_slice(&read_bytes(response)?)
        .map_err(|e| err(format!("the server's response was not JSON: {e}")))
}

/// The error for a non-2xx response, from its `{error}` / `{message}`, or from
/// the problem document the modules answer with (RFC 9457: `title`, `detail`).
/// The raw body is the last resort, and only because an unrecognised shape
/// should still say something.
fn api_error(status: u16, body: &[u8]) -> CliError {
    let parsed: Value = serde_json::from_slice(body).unwrap_or_default();
    let field = |name: &str| {
        parsed
            .get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    // A problem document's `detail` is the sentence written for the reader;
    // `title` only classifies it, so it is a prefix rather than a substitute.
    let problem = match (field("title"), field("detail")) {
        (Some(title), Some(detail)) => Some(format!("{title}: {detail}")),
        (_, Some(detail)) => Some(detail),
        (Some(title), None) => Some(title),
        _ => None,
    };
    let message = field("error")
        .or_else(|| field("message"))
        .or(problem)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
    let message = if message.is_empty() {
        format!("the API answered HTTP {status}")
    } else if status == 401 {
        format!("{message} — run `livingbrain login`")
    } else {
        message
    };
    CliError::Api(message)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::io;
    use std::time::Duration;

    use super::{
        Attempt, CliError, CliResult, Poll, UPLOAD_ATTEMPTS, is_transient, jittered, retrying,
    };

    fn transient() -> Attempt<u32> {
        Attempt::Transient {
            error: CliError::Api("reset".to_owned()),
            after: None,
        }
    }

    #[test]
    fn only_a_reset_refusal_close_or_timeout_is_transient() {
        for kind in [
            io::ErrorKind::ConnectionReset,
            io::ErrorKind::ConnectionRefused,
            io::ErrorKind::ConnectionAborted,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::TimedOut,
        ] {
            assert!(
                is_transient(&ureq::Error::Io(io::Error::from(kind))),
                "{kind:?}"
            );
        }
        assert!(is_transient(&ureq::Error::ConnectionFailed));
        assert!(is_transient(&ureq::Error::Timeout(ureq::Timeout::Global)));
        assert!(!is_transient(&ureq::Error::HostNotFound));
        assert!(!is_transient(&ureq::Error::Io(io::Error::from(
            io::ErrorKind::PermissionDenied
        ))));
        assert!(!is_transient(&ureq::Error::BadUri("no host".to_owned())));
    }

    #[test]
    fn a_transient_failure_is_retried_until_it_succeeds() {
        let tries = Cell::new(0);
        let pauses = Cell::new(0);
        let result = retrying(
            || {
                tries.set(tries.get() + 1);
                if tries.get() < 3 {
                    transient()
                } else {
                    Attempt::Done(7)
                }
            },
            |_, _| pauses.set(pauses.get() + 1),
        );
        assert_eq!(result.expect("succeeds on the third try"), 7);
        assert_eq!(tries.get(), 3);
        assert_eq!(pauses.get(), 2, "one pause before each retry");
    }

    #[test]
    fn a_final_failure_is_not_retried() {
        let tries = Cell::new(0);
        let result: Result<u32, CliError> = retrying(
            || {
                tries.set(tries.get() + 1);
                Attempt::Fatal(CliError::Api("refused".to_owned()))
            },
            |_, _| panic!("a final failure must not pause"),
        );
        assert!(result.is_err());
        assert_eq!(tries.get(), 1);
    }

    #[test]
    fn a_transient_failure_gives_up_after_the_last_try_with_its_error() {
        let tries = Cell::new(0);
        let result: Result<u32, CliError> = retrying(
            || {
                tries.set(tries.get() + 1);
                transient()
            },
            |_, _| {},
        );
        assert_eq!(tries.get(), UPLOAD_ATTEMPTS);
        assert_eq!(result.expect_err("gives up").to_string(), "reset");
    }

    #[test]
    fn the_server_hint_reaches_the_pause() {
        let hints = std::cell::RefCell::new(Vec::new());
        let _: CliResult<u32> = retrying(
            || Attempt::Transient {
                error: CliError::Api("busy".to_owned()),
                after: Some(9),
            },
            |tried, after| hints.borrow_mut().push((tried, after)),
        );
        assert_eq!(*hints.borrow(), vec![(1, Some(9)), (2, Some(9))]);
    }

    #[test]
    fn jitter_stays_within_a_quarter_either_way() {
        let base = Duration::from_secs(4);
        for _ in 0..200 {
            let pause = jittered(base);
            assert!(pause >= Duration::from_secs(3), "{pause:?}");
            assert!(pause < Duration::from_secs(5), "{pause:?}");
        }
    }

    #[test]
    fn a_token_poll_is_read_by_the_rfc_8628_error_code() {
        assert!(matches!(
            Poll::from_body(br#"{"access_token":"t"}"#).expect("issued"),
            Poll::Issued(t) if t == "t"
        ));
        assert!(matches!(
            Poll::from_body(br#"{"error":"authorization_pending"}"#).expect("pending"),
            Poll::Pending
        ));
        assert!(matches!(
            Poll::from_body(br#"{"error":"slow_down"}"#).expect("slow down"),
            Poll::SlowDown
        ));
        assert!(Poll::from_body(br#"{"error":"expired_token"}"#).is_err());
        assert!(Poll::from_body(br#"{"error":"access_denied"}"#).is_err());
    }
}
