//! The Living Brain Worker: the composition root, and the only crate that
//! names a runtime and a vendor SDK.
//!
//! It mounts every `livingbrain-*` module into one Cratefield harness and
//! serves it on Cloudflare Workers: the empty `canary` module from the
//! scaffold; `workspaces` — one tenant per Slack workspace, sign in with
//! Slack, and the member mirror the Events webhook (issue #6) refreshes;
//! and `models` — bring your own model per workspace and role, with the
//! capability check issue #10 asks for; `pages` — versioned entity pages,
//! Markdown bodies in R2 and metadata, links and history in D1 (issue #15);
//! and the harness `waitlist` module —
//! the early-access list behind issue #23: joins from livingbrain.wiki, double
//! opt-in mail through Owlpost, and the CSV export behind the admin token;
//! and the CLI surface (issues #121 and #77) — `notes`, `search`, `ask`,
//! `export` and `sources` from `livingbrain-api`, the five modules behind
//! `livingbrain`'s client contract, whose paths `fetch` diverts through the
//! harness router itself (`serve_cli` below); and `tokens` with the harness
//! `device-auth` grant beside it — the personal access tokens behind issue
//! #72, minted by a device that has no browser to sign in with, which are
//! also what the MCP endpoint now accepts.
//! `wrangler.toml` and the D1 migrations are beside this crate.
//!
//! `workspaces` requires `Db`, `Signer`, `HttpClient`, `Clock` and `IdGen`.
//! The runtime provides the last four unconditionally (`Signer` from
//! `HARNESS_SECRET`) and `Db` from the `.db("DB")` binding below, so
//! `Harness::build` refuses a composition that leaves any of them unwired.
//!
//! Mail goes through Owlpost (`cratefield-adapter-owlpost`); the waitlist's
//! confirm mails render through `themed_templates(&mail_theme())`.
#![forbid(unsafe_code)]

use cratefield_adapter_owlpost::Owlpost;
use cratefield_adapter_turnstile::Turnstile;
use cratefield_core::axum::body::{Body, to_bytes};
use cratefield_core::axum::http::{
    HeaderMap, HeaderValue, Method as HttpMethod, Request as HttpRequest, header,
};
use cratefield_core::axum::response::Response as AxumResponse;
use cratefield_core::{
    Blob, Database, Harness, HarnessBuilder, Module, Ports, Problem, RandomBytes, RandomError,
    Template, Venture,
};
use cratefield_kms::{Kms, WorkerSecretKms};
use cratefield_mail_templates::MailTheme;
use cratefield_module_device_auth::{DeviceAuth, DeviceClient};
use cratefield_module_waitlist::{Waitlist, themed_templates};
use cratefield_runtime_cloudflare::{
    Cloudflare, ContextDefer, FetchClient, WorkersClock, install_tracing, serve, serve_scheduled,
};
use livingbrain_api::{Ask, Export, Notes, PagesRebind, Search, Sources, Wiki};
use livingbrain_canary::Canary;
use livingbrain_mcp::{Asker, AuthError, BearerAuth};
use livingbrain_models::Models;
use livingbrain_pages::{Answered, Answers, PageAnswers, PageStore, Pages};
use livingbrain_tokens::{DevicePorts, Tokens};
use livingbrain_tools::Tools;
use livingbrain_usage::Usage;
use livingbrain_workspaces::{
    Admit, EmailLedger, Exchange, LocalTurns, Pending, Turn, TurnError, Turns, Workspaces,
};
use std::sync::{Arc, OnceLock};
use time::OffsetDateTime;
use tower::ServiceExt;
use worker::{Context, Env, ObjectNamespace, Request, Response, event};

mod conversation;

use conversation::{CONVERSATIONS, DurableTurns};

/// The product slug the site's waitlist form joins (issue #23).
pub const WAITLIST_PRODUCT: &str = "livingbrain";

/// The API's own origin, for `Venture::public_url` alone. The OAuth discovery
/// documents deliberately do **not** use it: they name the origin each
/// request reached, so one binary serves `api.`, `staging-api.`, `mcp.` and
/// a `wrangler dev` on `localhost:8787` correctly (issue #72).
const API_BASE: &str = "https://api.livingbrain.wiki";

/// The client the device grant issues to (issue #72). The grant refuses a
/// `client_id` it has not been told about, so a name here is the whole of
/// the allow-list.
const DEVICE_CLIENT: &str = "livingbrain-cli";

/// The venture as this Worker serves it, and as a native test mounts it.
///
/// The second argument is the **apex** domain. The `waitlist` module derives
/// both `https://api.{domain}` for the confirm link and `no-reply@send.{domain}`
/// for the sender from it, so passing `api.livingbrain.wiki` here would
/// double the label (`api.api…`, `send.api…`). `public_url` is the API's own
/// address, spelled out rather than derived.
///
/// The environment is left to the deployment, which declares it through
/// `ENV` in `wrangler.toml` (`[vars] ENV = "production"`), not hardcoded
/// here: `Harness::build` takes no config, so a compiled-in production would
/// refuse to build rather than serve and say what it is missing.
#[must_use]
pub fn venture() -> Venture {
    Venture::new("livingbrain", "livingbrain.wiki")
        .public_url(API_BASE)
        .cors_origins(["https://livingbrain.wiki", "https://api.livingbrain.wiki"])
}

/// The waitlist module as this venture configures it: the single product
/// `livingbrain`, and the post-confirm landing on the site — the module's
/// default status page is at `/ui/waitlist/status`, which this Worker does
/// not serve. A function rather than a `const` so a native test mounts
/// exactly what the Worker serves.
#[must_use]
pub fn waitlist_module() -> Waitlist {
    Waitlist::new()
        .products([WAITLIST_PRODUCT])
        .status_redirect("https://livingbrain.wiki/")
}

/// The waitlist confirm mails rendered in this venture's theme, so a native
/// test composes exactly what the Worker serves.
#[must_use]
pub fn templates() -> Vec<(String, Box<dyn Template>)> {
    themed_templates(&mail_theme())
}

/// Turnstile when `TURNSTILE_SECRET` is present on the Worker `Env`, else no
/// `Captcha` port at all (the kit's fail-closed gate then refuses the join
/// form in production). The hostname is bound deliberately: an unbound
/// adapter reports itself not effectively configured, so readiness would
/// refuse the composition. The form is only ever solved on the apex.
fn build_captcha(env: &Env) -> Option<Turnstile> {
    let secret = env
        .secret("TURNSTILE_SECRET")
        .ok()
        .map(|secret| secret.to_string())
        .filter(|secret| !secret.is_empty())?;
    Some(
        Turnstile::new(Arc::new(FetchClient), Arc::new(WorkersClock), secret)
            .expected_hostname("livingbrain.wiki"),
    )
}

static INSTANCE: OnceLock<(Harness, Cloudflare, TurnsCell)> = OnceLock::new();

/// The sender when `MAIL_FROM` is unset: a person-readable name on the
/// venture's own domain, which must be verified in Owlpost.
const DEFAULT_MAIL_FROM: &str = "Living Brain <hello@livingbrain.wiki>";

/// The venture's mail theme, parsed from `mail-theme.json` beside this file.
///
/// The parse is a runtime one; the mail tests read the same file on every run,
/// so a malformed edit fails in CI rather than at the first send.
#[must_use]
pub fn mail_theme() -> MailTheme {
    MailTheme::from_json(include_str!("mail-theme.json"))
        .expect("mail-theme.json is a valid MailTheme")
}

/// The mail settings resolved from the Worker's `Env`.
///
/// Split from [`build`] so a native test, which has no `worker::Env`, can still
/// compose the whole harness with the keyless fallback.
struct MailSettings {
    /// The `OWLPOST_API_KEY` secret; `None` is no key, which sends nothing.
    api_key: Option<String>,
    /// The `MAIL_FROM` sender; [`DEFAULT_MAIL_FROM`] when the binding is unset.
    from: String,
    /// The optional `MAIL_REPLY_TO` address.
    reply_to: Option<String>,
}

impl MailSettings {
    /// Reads the mail configuration from `env`. A binding that is missing,
    /// empty or blank is treated as unset, so a stray empty secret cannot
    /// become an empty sender or key.
    fn from_env(env: &Env) -> Self {
        Self {
            api_key: non_blank(
                env.secret("OWLPOST_API_KEY")
                    .ok()
                    .map(|key| key.to_string()),
            ),
            from: non_blank(env.var("MAIL_FROM").ok().map(|value| value.to_string()))
                .unwrap_or_else(|| DEFAULT_MAIL_FROM.to_owned()),
            reply_to: non_blank(env.var("MAIL_REPLY_TO").ok().map(|value| value.to_string())),
        }
    }
}

/// `None` for a value that is absent, blank or only whitespace; otherwise the
/// trimmed value, so a secret carrying a trailing newline cannot become a key
/// that Owlpost rejects as unauthorized.
fn non_blank(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// Builds the composed harness and the runtime it was validated against.
///
/// One runtime, built once: `Harness::build` checks every module's
/// `requires()` against the ports of the instance it is handed, so a second,
/// separately-built instance would validate something other than what serves.
/// The same instance is cloned into the harness and handed to `serve`.
///
/// Takes the already-resolved [`MailSettings`], captcha and key custodian
/// rather than an `Env` so the test below can compose it natively;
/// [`instance`] reads the `Env` and calls this.
fn build(
    mail: MailSettings,
    captcha: Option<Turnstile>,
    kms: Option<Arc<dyn Kms>>,
) -> (Harness, Cloudflare, TurnsCell) {
    let runtime = Cloudflare::new()
        // The bindings `wrangler.toml` declares. `workspaces` and `waitlist`
        // read their tables through the D1 binding, `pages` its bodies through
        // R2; KV is wired ahead of a module that needs it. The Workers Rate
        // Limiting binding guards the `waitlist` module's public write and
        // admin routes.
        .db("DB")
        .blob("R2")
        .kv("KV")
        .rate_limiter("RATE_LIMITER")
        // Installed with or without a key, so `Mailer` is always provided.
        .mailer(Owlpost::new(
            Arc::new(FetchClient),
            Arc::new(WorkersClock),
            mail.api_key,
            mail.from,
            mail.reply_to,
        ))
        // **No `Classifier`, and that is a known gap, not an oversight**
        // (issue #123). `Cloudflare::classifier_arc` would take one, but both
        // adapters `cratefield-core` names for it need something this Worker
        // has no binding for: `ClassifierLlm` is a `Classifier` over a
        // `TextModel`, and this venture wires no `text_model` either — there
        // is no `AI` binding in `wrangler.toml` and no `models`-module
        // connection is a `TextModel`. `TypeSafe` wants a `TYPESAFE_API_KEY`
        // secret this deployment does not declare. Wiring either means a new
        // secret or a new binding and the `docs/deploy.md` entry that goes
        // with it, which is a decision for the issue that owns the model
        // wiring, not one to guess at here.
        //
        // Until then the Slack agent runs the unwired-judge policy: it
        // answers an @mention and stays silent in a DM. That is the correct
        // behaviour for a deployment with no judge, and the DM half of
        // issue #123 does not work in this deployment until one is wired.
        ;
    let runtime = match captcha {
        Some(captcha) => runtime.captcha(captcha),
        None => runtime,
    };
    // The Slack agent's answer seam (issue #123), filled by the pages
    // module's own closure and read per event. See [`AnswersCell`].
    let answers = AnswersCell::default();
    // The agent's turn seam (issue #7): the conversation objects, published
    // per request and answered through the per-isolate coordinator until
    // the first one lands. See [`TurnsCell`].
    let turns = TurnsCell::default();
    // Issue #72: one handle shared by the device grant's hooks and the tokens
    // module, built before the harness because the grant takes its hooks at
    // composition time and the ports only exist per request.
    let device_ports = DevicePorts::new();
    let harness = {
        let builder = Harness::builder()
            .venture(venture())
            // The waitlist module's confirm mails in this venture's theme.
            .templates(templates())
            .module(Canary::new())
            .module(
                Workspaces::new()
                    .answering(Arc::new(answers.clone()))
                    .taking_turns(Arc::new(turns.clone()))
                    .email_ledger(Arc::new(UsageLedger)),
            )
            .module(Models::new())
            .module(Tools::new())
            .module(device_auth_module(&device_ports))
            .module(tokens_module(device_ports))
            .module(pages_module(kms.clone(), &answers))
            .module(waitlist_module())
            // The cost ledger (issue #12): `GET /v1/usage/daily`, backed by
            // the records the models and workspaces modules write through
            // their seams. It needs only what every module here gets — the
            // `DB` binding, the Workers clock and the `HARNESS_SECRET`
            // signer — so the boot gate accepts it where the others stand.
            .module(Usage::new());
        // The CLI modules mount with the same key ring the pages surface gates
        // on, so they join the fold the same way pages does.
        let builder = cli_modules(kms)
            .into_iter()
            .fold(builder, HarnessBuilder::module_arc);
        builder
            .runtime(runtime.clone())
            .build()
            .expect("the livingbrain harness is valid")
    };
    (harness, runtime, turns)
}

/// The five CLI modules (issues #121 and #77): the routes `livingbrain`'s
/// client speaks — `POST /v1/notes`, `GET /v1/search`, `POST /v1/ask`,
/// `GET /v1/export`, `GET /v1/sources/{id}` — each mounted at its own name
/// under the harness rule, every one constructed with the same [`Wiki`] over
/// the key custodian and the [`TokenBearer`].
///
/// The blob a module's store reads is not decided here — the composition
/// roots the per-request port layer on the pages key space instead, so the
/// page bodies stay physical `pages/` objects no matter which route wrote
/// them ([`PagesRebind`], planted in [`serve_cli`]). With no key custodian
/// there is nothing to seal a page with, so — like [`pages_module`] — none
/// of the five is mounted and every other route keeps working.
fn cli_modules(kms: Option<Arc<dyn Kms>>) -> Vec<Arc<dyn Module>> {
    let Some(kms) = kms else {
        return Vec::new();
    };
    let auth: Arc<dyn BearerAuth> = Arc::new(TokenBearer);
    let wiki = Wiki::new(kms, Arc::clone(&auth));
    vec![
        Arc::new(Notes::new(wiki.clone())),
        Arc::new(Search::new(wiki.clone())),
        Arc::new(Ask::new(wiki.clone())),
        Arc::new(Export::new(wiki.clone())),
        Arc::new(Sources::new(wiki)),
    ]
}

/// The device grant (issue #72) for a client with no browser to sign in
/// with: the CLI. Its hooks are the ones the `tokens` module supplies — a
/// signed-in person approves, and an approved device gets a PAT.
fn device_auth_module(ports: &DevicePorts) -> DeviceAuth {
    DeviceAuth::builder()
        .client(DeviceClient::new(DEVICE_CLIENT))
        .random(WorkersRandom)
        .approver(ports.approver())
        .issuer(ports.issuer())
        .build()
}

/// The tokens module (issue #72): the routes a settings page drives, and
/// the two `/.well-known` documents.
///
/// No `api_base`: both documents are written under the origin each request
/// reached, so staging, `wrangler dev` and `mcp.` are each named correctly
/// without this composition knowing which one it is serving.
fn tokens_module(ports: DevicePorts) -> Tokens {
    Tokens::new().device_ports(ports)
}

/// The cost ledger behind the workspaces module's [`EmailLedger`] seam
/// (issue #12): one row increment per magic-link mail the module sends.
///
/// The seam exists because the dependency graph runs the other way — the
/// usage module reaches the workspaces module through `tokens`
/// (`usage` → `tokens` → `workspaces`), so the workspaces module cannot
/// depend back on `usage` without a cycle Cargo refuses. The composition,
/// which sees both, implements the seam over `livingbrain_usage` and
/// injects it, exactly as it does the pages answer seam.
///
/// The failure is dropped rather than returned because the seam's contract
/// is best-effort: a lost count must never fail the mail it counted.
struct UsageLedger;

#[async_trait::async_trait]
impl EmailLedger for UsageLedger {
    async fn record_email(&self, db: &dyn Database, now: OffsetDateTime, workspace_id: &str) {
        let _ = livingbrain_usage::record_email(db, now, workspace_id).await;
    }
}

/// The entropy source the device grant draws its code pair from — the
/// isolate's own CSPRNG, the same call the tokens module makes.
///
/// A failed draw panics rather than returning an error, and that is the
/// only honest answer available: [`RandomError`]'s field is private, so
/// nothing outside `cratefield-core` can construct one to return.
#[derive(Clone, Copy, Debug)]
struct WorkersRandom;

impl RandomBytes for WorkersRandom {
    fn fill(&self, dest: &mut [u8]) -> Result<(), RandomError> {
        match getrandom::fill(dest) {
            Ok(()) => Ok(()),
            Err(err) => panic!("the isolate's entropy source failed: {err}"),
        }
    }
}

/// The pages module as this venture composes it, with the MCP server and the
/// source importer nested in it (issues #24 and #81) and the page routes the
/// CLI's `livingbrain page` speaks merged at the mount root (issue #121) —
/// `GET /v1/pages`, `GET|PUT /v1/pages/{slug}`. The harness scopes `Blob` per
/// module name, so only a surface built from the pages module's own context
/// can open a body those pages write.
///
/// With no key custodian there is no body anyone may open, so neither surface
/// is mounted at all and every other route keeps working — a deployment
/// missing a secret gets a working venture, not a Worker that panics on its
/// first request.
///
/// The same closure also builds the [`Answers`] the Slack agent answers
/// through (issue #123), for the same reason and from the same ports. It
/// cannot be handed to `workspaces` directly — the two modules are composed
/// before any context exists — so it goes in through [`AnswersCell`], which
/// is filled here and read per event. With no key custodian this closure
/// never runs and the cell stays empty for the isolate's life; the agent
/// reports that rather than answering nobody in silence.
fn pages_module(kms: Option<Arc<dyn Kms>>, answers: &AnswersCell) -> Pages {
    let Some(kms) = kms else {
        return Pages::new();
    };
    // Each surface gets its own handle to the same custodian and the same
    // credential resolver: one key, so a body either of them seals is a body
    // the other can open.
    let auth: Arc<dyn BearerAuth> = Arc::new(TokenBearer);
    let mcp_kms = Arc::clone(&kms);
    let mcp_auth = Arc::clone(&auth);
    let surface_kms = Arc::clone(&kms);
    let surface_auth = Arc::clone(&auth);
    let cell = answers.clone();
    Pages::new()
        .surface(move |ctx| {
            livingbrain_api::pages_routes(ctx, Arc::clone(&surface_kms), Arc::clone(&surface_auth))
        })
        .nest("/mcp", move |ctx| {
            // Read apart from `ctx`, which the MCP router below takes whole.
            if let (Some(db), Some(blob), Some(clock), Some(id_gen)) = (
                ctx.ports.db.clone(),
                ctx.ports.blob.clone(),
                ctx.ports.clock.clone(),
                ctx.ports.id_gen.clone(),
            ) {
                cell.publish(Arc::new(PageAnswers::new(
                    PageStore::new(db, blob, Arc::clone(&mcp_kms), clock, id_gen),
                    WIKI_BASE,
                )));
            }
            livingbrain_mcp::router(ctx, Arc::clone(&mcp_kms), Arc::clone(&mcp_auth))
        })
        .nest("/sources", move |ctx| {
            livingbrain_mcp::sources_router(ctx, Arc::clone(&kms), Arc::clone(&auth))
        })
}

/// Where the pages are published. A citation is an absolute link, so this is
/// the site's own origin rather than the API's.
const WIKI_BASE: &str = "https://livingbrain.wiki";

/// The [`Answers`] the composition could not build yet.
///
/// **Why a cell at all.** A [`PageStore`] needs the pages module's `Blob`,
/// and the harness hands each module a blob scoped to *its own name*: a
/// `ScopedBlob` the `workspaces` module built itself would live under
/// `workspaces/`, where no page body is ever written, and the workspaces
/// module does not even declare `Port::Blob`. So the store cannot be built
/// there at all. It is built inside the pages module's own closure, which is
/// handed a `ModuleContext` no earlier than the request that runs it, and
/// published here — the one seam that reaches `workspaces` without either
/// module reaching into the other.
///
/// Publishing replaces rather than first-wins because a Worker holds a
/// request's D1 and R2 handles and has no business holding the first one's
/// for the isolate's life. Every rebuild is equivalent — the same bindings
/// behind it — so a concurrent reader sees a store that answers the same.
///
/// The slot is empty until some request runs the closure, which the Slack
/// agent reads as [`AnswerError::Unavailable`] and reports rather than
/// answering from nothing.
#[derive(Clone, Default)]
struct AnswersCell(Arc<std::sync::RwLock<Option<Arc<dyn Answers>>>>);

impl AnswersCell {
    fn publish(&self, answers: Arc<dyn Answers>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = Some(answers);
        }
    }

    fn get(&self) -> Option<Arc<dyn Answers>> {
        self.0.read().ok().and_then(|slot| slot.clone())
    }
}

#[async_trait::async_trait]
impl Answers for AnswersCell {
    async fn answer(
        &self,
        asker: &livingbrain_pages::Asker,
        question: &str,
    ) -> Result<Answered, livingbrain_pages::AnswerError> {
        self.get()
            .ok_or(livingbrain_pages::AnswerError::Unavailable)?
            .answer(asker, question)
            .await
    }
}

/// The [`Turns`] the composition could not build yet (issue #7), the same
/// shape as [`AnswersCell`]: the conversation objects are a Worker binding,
/// which resolves from an `Env` that exists no earlier than the request
/// carrying it — so `fetch` publishes the `CONVERSATIONS` namespace per
/// request and the agent loop reads it back through here. Until one lands,
/// the cell delegates to a plain [`LocalTurns`], the per-isolate
/// coordinator: it serialises every turn *within this isolate* (what a
/// native test gets) and the lease makes the cross-isolate gap visible
/// rather than silent.
#[derive(Clone, Default)]
struct TurnsCell {
    slot: Arc<std::sync::RwLock<Option<Arc<dyn Turns>>>>,
    /// The per-isolate coordinator served until a namespace is published.
    local: Arc<LocalTurns>,
}

impl TurnsCell {
    fn publish(&self, namespace: ObjectNamespace) {
        if let Ok(mut slot) = self.slot.write() {
            *slot = Some(Arc::new(DurableTurns::new(namespace)));
        }
    }

    /// The coordinator this request answers through: the conversation objects
    /// when one has been published, the per-isolate one until then.
    fn delegate(&self) -> Arc<dyn Turns> {
        self.slot
            .read()
            .ok()
            .and_then(|slot| slot.clone())
            .unwrap_or_else(|| self.local.clone())
    }
}

#[async_trait::async_trait]
impl Turns for TurnsCell {
    async fn arrive(&self, key: &str, pending: Pending, now: i64) -> Result<Admit, TurnError> {
        self.delegate().arrive(key, pending, now).await
    }

    async fn finish(
        &self,
        key: &str,
        lease: u64,
        exchange: Option<Exchange>,
        now: i64,
    ) -> Result<Option<Turn>, TurnError> {
        self.delegate().finish(key, lease, exchange, now).await
    }
}

/// The key custodian page bodies are sealed and opened with (issue #43), over
/// the `HARNESS_KEK_CURRENT` / `HARNESS_KEK_V<n>` secret ring — the same
/// names and the same shape the page store's own tests use. `None` when the
/// ring is absent or malformed.
fn build_kms(env: &Env) -> Option<Arc<dyn Kms>> {
    let kms = WorkerSecretKms::from_lookup(|name| {
        env.secret(name)
            .ok()
            .map(|value| value.to_string())
            .filter(|value| !value.is_empty())
    })
    .ok()?;
    Some(Arc::new(kms))
}

/// The [`BearerAuth`] for the MCP endpoint (issue #24), now resolving a real
/// credential (issue #72): a personal access token from `livingbrain login`,
/// handed to the tokens module's one `authenticate` — the same entry point
/// every route in this venture uses — so a cookie and a token resolve to the
/// same member here as everywhere else, and the scope subset travels with
/// the answer instead of being re-derived per request.
struct TokenBearer;

/// RFC 6265 §4.1.1 `cookie-octet`: US-ASCII except the separators and the
/// controls. A session token is base64url, so it never needs one of these.
fn is_cookie_octet(byte: u8) -> bool {
    matches!(byte,
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
        | b'!' | b'#'..=b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}

#[async_trait::async_trait]
impl BearerAuth for TokenBearer {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        // The value is spliced into a request, so anything outside RFC 6265's
        // cookie-octet set is refused here rather than splitting the header,
        // or smuggling a second credential in, on the way in.
        if !token.bytes().all(is_cookie_octet) {
            return Err(AuthError::Rejected);
        }
        let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) else {
            return Err(AuthError::Rejected);
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);
        livingbrain_tokens::authenticate(ports, &headers)
            .await
            .map(|member| Asker {
                workspace_id: member.workspace_id,
                user_id: member.user_id,
                token_scopes: member.scopes,
            })
            .map_err(|_| AuthError::Rejected)
    }
}

/// The process-wide instance, built on the first request from that request's
/// `Env`. Worker bindings are static for a deployment, so the first `Env` is
/// every `Env`; secrets are read from it because `std::env` is empty on
/// Workers.
fn instance(env: &Env) -> &'static (Harness, Cloudflare, TurnsCell) {
    INSTANCE.get_or_init(|| {
        build(
            MailSettings::from_env(env),
            build_captcha(env),
            build_kms(env),
        )
    })
}

/// The five CLI paths `fetch` diverts to [`serve_cli`] — the routes the
/// five [`cli_modules`] mount, minus `/v1/pages`, which the pages module
/// serves through the ordinary [`serve`] path like every other route.
const CLI_ROUTES: [&str; 5] = [
    "/v1/notes",
    "/v1/search",
    "/v1/ask",
    "/v1/export",
    "/v1/sources",
];

/// Whether this request is one of the five CLI paths the venture diverts,
/// and the surface is mounted at all. The mount is gated on the key ring
/// (see [`cli_modules`]), so the presence of the `notes` module is the same
/// gate — a deployment without the ring takes these paths through the
/// ordinary `serve`, which answers 404, exactly as a composition without
/// the modules should.
///
/// Four of the five are single endpoints and match exactly — a subroute
/// under them is nobody's route, and exact matching keeps a future one from
/// silently changing handler stack. The citation route is the exception: it
/// carries the source id as its path segment, and matching only the bare
/// `/v1/sources` would drop every real citation on the floor.
fn serves_cli(harness: &Harness, path: &str) -> bool {
    let diverted = CLI_ROUTES.iter().any(|route| {
        path == *route || (*route == "/v1/sources" && path.starts_with("/v1/sources/"))
    });
    diverted && harness.modules().iter().any(|m| m.name() == "notes")
}

#[event(fetch)]
/// Worker fetch entry point.
///
/// # Errors
///
/// Propagates `worker::Error` from the harness router.
pub async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let (harness, runtime, turns) = instance(&env);
    // Issue #7: publish this request's `CONVERSATIONS` namespace, so the
    // agent loop's turn calls reach the conversation objects. The publish is
    // per request for the same reason `AnswersCell`'s is — the `Env`-derived
    // handle belongs to the request that read it — and when the binding is
    // missing the cell falls back to the per-isolate coordinator rather than
    // dropping the turn on the floor.
    if let Ok(namespace) = env.durable_object(CONVERSATIONS) {
        turns.publish(namespace);
    }
    if serves_cli(harness, req.path().as_str()) {
        return serve_cli(harness, runtime, req, env, ctx).await;
    }
    serve(harness, runtime, req, env, ctx).await
}

/// Serves one of the five CLI paths ([`CLI_ROUTES`]) through the harness
/// router directly.
///
/// Why divert at all: the pinned harness has no seam to re-point one
/// module's blob view at another module's key space, and workerd ties an
/// env-derived I/O object to the request that created it, so the composition
/// cannot hand the four modules a pages-scoped store at build time either
/// (`Cannot perform I/O on behalf of a different request context`). What the
/// harness *does* expose is the per-request bundle and the router itself,
/// both public. So this path re-derives the ports the way [`serve`] does,
/// plants a [`PagesRebind`] over the raw store — the harness then scopes
/// each module's view on top, and every `notes/…`, `search/…`, `ask/…`,
/// `export/…`, `sources/…` key lands on `pages/…` — and drives the full
/// router through a one-shot `tower` call. Every layer `serve` would apply
/// still stands:
/// CORS, the abuse floor, the per-route body ceilings, problem+json.
///
/// The body gate mirrors `serve`'s buffered plan: a declared
/// `content-length` over the module's ceiling is refused unread, a
/// declaration at or under it is buffered whole. A body with no usable
/// declaration is refused too — `serve` would read it capped through the
/// streaming layer, and rather than re-derive that machinery here (a
/// `!Send` bridge and a hand-rolled cap), the five CLI routes ask for a
/// declared length; the CLI and the web app always send one.
///
/// # Errors
///
/// `worker::Error` on conversion/transport failures; problem+json
/// responses are ordinary 4xx Worker responses.
async fn serve_cli(
    harness: &Harness,
    runtime: &Cloudflare,
    mut req: Request,
    env: Env,
    ctx: Context,
) -> worker::Result<Response> {
    install_tracing();
    let mut ports = runtime.ports(&env, Arc::new(ContextDefer(ctx)));
    // The port-layer planting: the four scopes land on the pages key space,
    // through handles derived this request. Keys outside the four prefixes
    // — the pages module's own — pass through unchanged.
    ports.blob = ports
        .blob
        .take()
        .map(|raw| Arc::new(PagesRebind::new(raw)) as Arc<dyn Blob>);
    // `ports` moves into `router()` below, and the ceiling lookup reads the
    // same config the module contexts were built with — clone the `Arc`
    // out first, the way `serve` does.
    let config = Arc::clone(&ports.config);
    let router = harness.router(ports);
    let problem_type_base = harness.venture().problem_type_base();

    let url = req.url()?;
    let path = url.path().to_owned();
    let method = HttpMethod::from_bytes(req.method().to_string().as_bytes())
        .map_err(|err| worker::Error::RustError(err.to_string()))?;
    let limit = harness.max_body_bytes(&path, config.as_ref());

    // Copied out so the header borrow ends before the body is read mutably.
    let declared = req.headers().get("content-length").ok().flatten();
    let bytes = match declared
        .as_deref()
        .and_then(|raw| raw.trim().parse::<usize>().ok())
    {
        // A declared length over the ceiling is refused unread.
        Some(len) if len > limit => {
            return response_to_worker(
                Problem::request_too_large().into_response_with_base(&problem_type_base),
            )
            .await;
        }
        // A declaration at or under the limit is buffered whole.
        Some(_) => req.bytes().await?,
        // No usable declaration: an empty body is fine (most GETs); a body
        // without one is refused, not trusted.
        None => match req.inner().body() {
            None => Vec::new(),
            Some(_) => {
                return response_to_worker(
                    Problem::request_too_large()
                        .with_detail(format!("a body on {path} must declare its content-length"))
                        .into_response_with_base(&problem_type_base),
                )
                .await;
            }
        },
    };

    let mut builder = HttpRequest::builder().method(method).uri(url.to_string());
    {
        let headers = req.headers();
        for (name, value) in headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
    }
    let buffered = builder
        .body(Body::from(bytes))
        .map_err(|err| worker::Error::RustError(err.to_string()))?;

    let response = router
        .oneshot(buffered)
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?;
    response_to_worker(response).await
}

/// The buffered response bridge, mirroring the runtime's own: 1 MiB is a
/// generous ceiling for what the five CLI routes answer (JSON, one zip).
async fn response_to_worker(response: AxumResponse) -> worker::Result<Response> {
    const MAX_RESPONSE_BUFFER: usize = 1024 * 1024;
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, MAX_RESPONSE_BUFFER)
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?;
    let mut out = Response::from_bytes(bytes.as_ref().to_vec())?.with_status(parts.status.as_u16());
    copy_headers(out.headers_mut(), &parts.headers);
    Ok(out)
}

/// Copies every response header onto the Worker response, per name: drop
/// what the `worker` builder pre-set (a default `content-type` from
/// `from_bytes`), then `append` every value — the same discipline the
/// runtime applies, kept for the same reasons (`Vary` plus `Set-Cookie`).
fn copy_headers(worker_headers: &mut worker::Headers, headers: &HeaderMap) {
    for name in headers.keys() {
        let _ = worker_headers.delete(name.as_str());
        for value in headers.get_all(name) {
            let _ = worker_headers.append(name.as_str(), value.to_str().unwrap_or_default());
        }
    }
}

#[event(scheduled)]
/// Worker scheduled (cron) entry point: fans a firing out over the same
/// composed harness's modules — the `waitlist` module prunes expired
/// confirmation claims here. The cron lives in `wrangler.toml`; a trigger on
/// another Worker never reaches this handler.
pub async fn scheduled(event: worker::ScheduledEvent, env: Env, ctx: worker::ScheduleContext) {
    let (harness, runtime, _) = instance(&env);
    serve_scheduled(harness, runtime, event, env, ctx).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratefield_kms::{Dek, LocalFileKms};

    /// The mail settings of a deployment with no Owlpost key: the adapter is
    /// wired but degrades to `NotConfigured`.
    fn unconfigured_mail() -> MailSettings {
        MailSettings {
            api_key: None,
            from: DEFAULT_MAIL_FROM.to_owned(),
            reply_to: None,
        }
    }

    /// `build()` panics if the composition is invalid, so reaching past it is
    /// the assertion: `workspaces` requires `Db`, `Signer`, `HttpClient`,
    /// `Clock` and `IdGen`, and `Harness::build` refuses a runtime that does
    /// not provide one of them.
    #[test]
    fn the_composition_mounts_every_module() {
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)));
        let names: Vec<&str> = harness
            .modules()
            .iter()
            .map(|module| module.name())
            .collect();
        assert!(names.contains(&"canary"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
        assert!(names.contains(&"models"), "{names:?}");
        assert!(names.contains(&"tools"), "{names:?}");
        assert!(names.contains(&"pages"), "{names:?}");
        assert!(names.contains(&"waitlist"), "{names:?}");
        for route in CLI_ROUTES {
            assert!(serves_cli(&harness, route), "{route}");
        }
        // Issue #72: the grant a CLI logs in through, and its credential.
        assert!(names.contains(&"device-auth"), "{names:?}");
        assert!(names.contains(&"tokens"), "{names:?}");
    }

    /// The citation route carries the source id as its path segment, so the
    /// divert matches the prefix — a real citation (`/v1/sources/<ulid>`) must
    /// reach the one handler stack that plants the pages-rooted blob view.
    /// The other four CLI paths stay exact: nothing mounts under them, and a
    /// subpath there must keep falling through to `serve` unchanged.
    #[test]
    fn the_sources_route_diverts_with_its_id_segment() {
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)));
        assert!(
            serves_cli(&harness, "/v1/sources/01HZZZBBBBBBBBBBBBBBBBBBB"),
            "a citation must divert to serve_cli"
        );
        assert!(
            serves_cli(&harness, "/v1/sources"),
            "the bare path still diverts"
        );
        assert!(
            !serves_cli(&harness, "/v1/notes/01HZZZBBBBBBBBBBBBBBBBBBB"),
            "the single-endpoint CLI paths stay exact"
        );
        assert!(
            !serves_cli(&harness, "/v1/sourcesother/01HZZZBBBBBBBBBBBBBBBBBBB"),
            "a name that merely starts alike is not the citation route"
        );
    }

    /// A token carrying a cookie delimiter is refused before it is spliced
    /// into a `Cookie` header, so it cannot smuggle a second cookie in.
    #[test]
    fn a_token_carrying_a_cookie_delimiter_is_not_a_cookie() {
        assert!(is_cookie_octet(b'a') && is_cookie_octet(b'9') && is_cookie_octet(b'-'));
        for byte in [b';', b',', b' ', b'"', b'\\', 0x7f, b'\n'] {
            assert!(!is_cookie_octet(byte), "{byte}");
        }
    }

    /// A deployment with no page key ring still composes: the MCP surface
    /// and the five CLI modules are not mounted, and every other route keeps
    /// working — the CLI paths fall through to `serve`, which answers 404.
    #[test]
    fn a_missing_key_ring_leaves_the_rest_of_the_composition_serving() {
        let (harness, _, _) = build(unconfigured_mail(), None, None);
        let names: Vec<&str> = harness.modules().iter().map(|m| m.name()).collect();
        assert!(names.contains(&"pages"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
        for module in ["notes", "search", "ask", "export", "sources"] {
            assert!(!names.contains(&module), "{names:?}");
        }
        for route in CLI_ROUTES {
            assert!(!serves_cli(&harness, route), "{route}");
        }
    }

    /// A blank or whitespace-only binding is unset, and a value keeps no
    /// surrounding whitespace, so a key carrying a newline cannot reach
    /// Owlpost.
    #[test]
    fn a_blank_binding_reads_as_unset() {
        assert_eq!(non_blank(None), None);
        assert_eq!(non_blank(Some(String::new())), None);
        assert_eq!(non_blank(Some(" \n".to_owned())), None);
        assert_eq!(non_blank(Some(" key\n".to_owned())), Some("key".to_owned()));
    }
}
