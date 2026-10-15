//! The per-conversation turn coordinator (issue #7), as a Durable Object.
//!
//! `#[durable_object]` is a `wasm_bindgen` export, so the class lives here in
//! the venture; the state machine and wire protocol are
//! [`livingbrain_workspaces`]' ([`Conversation`], [`Call`], [`apply`]) and
//! this object is *load, apply, store* and nothing else. One object per
//! conversation — `Message::conversation_key()` — so workerd routes every
//! message in a thread to the same object and runs its events one at a time
//! with input gates held: that is what serialises the turns and folds a
//! message arriving mid-turn into the next turn. Every [`Call`] carries the
//! `now` the agent loop read from its own `Clock` port, so the state machine
//! stays pure.

use livingbrain_workspaces::{Admit, Answer, Call, Exchange, Pending, Turn, TurnError, Turns};
#[cfg(target_arch = "wasm32")]
use livingbrain_workspaces::{Conversation, apply};
use worker::send::IntoSendFuture;
use worker::wasm_bindgen::JsValue;
#[cfg(target_arch = "wasm32")]
use worker::{DurableObject, Env, Response, State, durable_object};
use worker::{Method, ObjectNamespace, Request, RequestInit};

/// The one storage key the object keeps its conversation under: an object
/// is one conversation, so there is nothing else to name.
#[cfg(target_arch = "wasm32")]
const STATE_KEY: &str = "conversation";

/// The URL of the request that carries a [`Call`] to the object. The object
/// reads the body and nothing else, so the URL is a placeholder — but it
/// must parse, because `Request` refuses one that does not.
const TURN_URL: &str = "https://conversation.livingbrain.wiki/turn";

/// The venture's `CONVERSATIONS` binding, resolved once per request and
/// published into [`crate::TurnsCell`] — see `fetch` in `lib.rs` for why
/// per request.
pub(crate) const CONVERSATIONS: &str = "CONVERSATIONS";

/// One conversation's turns, served to the agent loop over HTTP.
///
/// The class exists only on the wasm build: `#[durable_object]` is a
/// `wasm_bindgen` export, and a native build — the tests, `clippy` — has
/// nothing to export it to. Everything that can be native already is: the
/// state machine lives in `livingbrain-workspaces`, and [`DurableTurns`]
/// below is plain worker API, so the gate costs the build nothing but the
/// class.
#[cfg(target_arch = "wasm32")]
#[durable_object(fetch)]
pub struct ConversationObject {
    state: State,
}

#[cfg(target_arch = "wasm32")]
impl DurableObject for ConversationObject {
    fn new(state: State, _env: Env) -> Self {
        Self { state }
    }

    /// One [`Call`], answered with one [`Answer`].
    ///
    /// # Errors
    ///
    /// When the body is not a [`Call`], or the conversation could not be
    /// loaded or stored. The caller reads that as a [`TurnError`] and
    /// degrades — answering unserialised on an arrive, stopping on a
    /// finish — rather than retrying into the same object.
    async fn fetch(&self, req: Request) -> worker::Result<Response> {
        let mut req = req;
        let call: Call = req.json().await?;
        // **Load, apply, store — with no outbound I/O in between.** The
        // state machine awaits nothing but this object's own storage, and
        // workerd's input gate keeps every other event off the object until
        // the handler yields, so the read-modify-write is atomic: this is
        // what makes the lease a real lock on the conversation.
        let mut conversation = self
            .state
            .storage()
            .get::<Conversation>(STATE_KEY)
            .await?
            .unwrap_or_default();
        let answer = apply(&mut conversation, call);
        self.state.storage().put(STATE_KEY, &conversation).await?;
        Response::from_json(&answer)
    }
}

/// The [`Turns`] the agent loop holds: every call is one request to the
/// conversation's object.
///
/// Stubs are created **per call**: workerd ties the I/O objects of one
/// request to that request, and the coordinator outlives any of them. Only
/// the namespace is kept, and it is re-resolved from the `Env` on every
/// request (see [`crate::TurnsCell`]).
pub(crate) struct DurableTurns {
    namespace: ObjectNamespace,
}

impl DurableTurns {
    pub(crate) fn new(namespace: ObjectNamespace) -> Self {
        Self { namespace }
    }

    /// Sends one [`Call`] to the conversation's object and reads the
    /// [`Answer`] back.
    ///
    /// # Errors
    ///
    /// [`TurnError`] when the call could not be encoded, or the object
    /// could not be reached or read.
    async fn call(&self, key: &str, call: &Call) -> Result<Answer, TurnError> {
        let stub = self
            .namespace
            .id_from_name(key)
            .and_then(|id| id.get_stub())
            .map_err(|err| TurnError(format!("the conversation is unreachable: {err}")))?;
        let body = serde_json::to_string(call)
            .map_err(|err| TurnError(format!("the turn call would not encode: {err}")))?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(JsValue::from_str(&body)));
        let request = Request::new_with_init(TURN_URL, &init)
            .map_err(|err| TurnError(format!("the turn call would not build: {err}")))?;
        // `fetch_with_request`'s future is not `Send` — a `JsFuture` under
        // the hood — and the `Turns` seam is a `dyn` trait whose futures
        // must be. The runtime's realtime driver wraps worker's I/O the same
        // way (`IntoSendFuture`): a Workers isolate is single-threaded, so
        // the promise never crosses one.
        let mut response = stub
            .fetch_with_request(request)
            .into_send()
            .await
            .map_err(|err| TurnError(format!("the conversation did not answer: {err}")))?;
        response
            .json::<Answer>()
            .into_send()
            .await
            .map_err(|err| TurnError(format!("the conversation's answer did not read: {err}")))
    }
}

#[async_trait::async_trait]
impl Turns for DurableTurns {
    async fn arrive(&self, key: &str, pending: Pending, now: i64) -> Result<Admit, TurnError> {
        match self.call(key, &Call::Arrive { pending, now }).await? {
            Answer::Arrived(admit) => Ok(admit),
            // A coordinator that answered an arrive with a finish is
            // broken, and guessing the other half would turn the fault
            // into a wrong decision.
            Answer::Finished(_) => Err(TurnError(
                "the conversation answered an arrive with a finish".to_owned(),
            )),
        }
    }

    async fn finish(
        &self,
        key: &str,
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    ) -> Result<Option<Turn>, TurnError> {
        match self
            .call(
                key,
                &Call::Finish {
                    lease,
                    exchange,
                    now,
                },
            )
            .await?
        {
            Answer::Finished(turn) => Ok(turn),
            Answer::Arrived(_) => Err(TurnError(
                "the conversation answered a finish with an arrive".to_owned(),
            )),
        }
    }
}
