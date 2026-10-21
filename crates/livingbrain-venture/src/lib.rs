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
//! `export`, `sources`, `facts` and `coding-agents` from `livingbrain-api`, the modules behind
//! `livingbrain`'s client contract, whose paths `fetch` diverts through the
//! harness router itself (`serve_cli` below); and `tokens` with the harness
//! `device-auth` grant beside it — the personal access tokens behind issue
//! #72, minted by a device that has no browser to sign in with, which are
//! also what the MCP endpoint now accepts, beside the `actions` audience a
//! ChatGPT custom GPT holds (issue #234), admitted on four read-only routes
//! alone.
//! `wrangler.toml` and the D1 migrations are beside this crate.
//!
//! `workspaces` requires `Db`, `Signer`, `HttpClient`, `Clock` and `IdGen`.
//! The runtime provides the last four unconditionally (`Signer` from
//! `HARNESS_SECRET`) and `Db` from the `.db("DB")` binding below, so
//! `Harness::build` refuses a composition that leaves any of them unwired.
//!
//! Mail goes through Owlpost (`cratefield-adapter-owlpost`); the waitlist's
//! confirm mails render through `themed_templates(&mail_theme())`, and the
//! workspaces sign-in mail through [`ThemedSignInMail`] over the same theme.
#![forbid(unsafe_code)]

use cratefield_adapter_owlpost::Owlpost;
use cratefield_adapter_turnstile::Turnstile;
use cratefield_adapter_typesafe::TypeSafe;
use cratefield_core::axum::Router;
use cratefield_core::axum::body::{Body, Bytes, to_bytes};
use cratefield_core::axum::extract::State as AxumState;
use cratefield_core::axum::http::Extensions;
use cratefield_core::axum::http::{
    HeaderMap, HeaderValue, Method as HttpMethod, Request as HttpRequest, header,
};
use cratefield_core::axum::middleware::{Next, from_fn_with_state};
use cratefield_core::axum::response::{IntoResponse as _, Response as AxumResponse};
use cratefield_core::{
    Blob, BoxStream, Classifier, Database, Harness, HarnessBuilder, Module, ModuleContext, Ports,
    Problem, RandomBytes, RandomError, Rendered, ResponseStream, ScheduledBudget, ScheduledLimits,
    ScheduledSplit, StreamError, Template, Venture,
};
use cratefield_kms::{Kms, WorkerSecretKms};
use cratefield_mail_templates::{MailTheme, Message as MailBody};
use cratefield_module_device_auth::{DeviceAuth, DeviceClient};
use cratefield_module_waitlist::{Waitlist, themed_templates};
use cratefield_runtime_cloudflare::{
    Cloudflare, ContextDefer, FetchClient, ScheduleDefer, WorkersClock, install_tracing, serve,
};
use futures_util::StreamExt;
use livingbrain_accounts::{Access, Agents, Audit};
use livingbrain_api::{
    AppTools, Apps, Ask, BudgetSettings, CodingAgents, Dreaming, Export, Facts, Graph, ModelJobs,
    Notes, PagesRebind, PublicPages, ResumeParkedOnMain, Search, SessionAuth, Sources, Wiki,
};
use livingbrain_canary::Canary;
use livingbrain_discord::DiscordModule;
use livingbrain_github_app::GithubApp;
use livingbrain_mcp::{Asker, AuthError, BearerAuth};
use livingbrain_models::Models;
use livingbrain_models::jev::{JevConnector, JevKey};
use livingbrain_pages::{Answered, Answers, PageAnswers, PageStore, Pages};
use livingbrain_tokens::{Authenticated, Avatars, DevicePorts, Me, OAuth, Tokens};
use livingbrain_tools::Tools;
use livingbrain_usage::Usage;
use livingbrain_workspaces::{
    Admit, AvatarRebind, EmailLedger, ErasureFailed, Exchange, LocalTurns, MemberRemoval, Pending,
    SignInMail, SignInMailInput, Turn, TurnError, Turns, WorkspaceErasure, Workspaces,
};
use std::sync::{Arc, OnceLock};
use time::OffsetDateTime;
use tower::ServiceExt;
use worker::{Context, Env, ObjectNamespace, Request, Response, event};

mod conversation;
mod device_pages;
// The streaming seam for model calls, over `worker::Fetch`.

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

/// Jev (TypeSafe's System One), the coding-agents compiler's optional
/// triage classifier, as a connector the models module and the compiler
/// build it through. Which key a workspace's compiles run on is decided per
/// workspace (`livingbrain_models::jev::effective`): its own switch (an
/// owner can turn Jev off), then its own TypeSafe key (Settings → Models →
/// Jev, sealed like a model key), then this deployment's `TYPESAFE_API_KEY`
/// secret, else none — and with none, the compiler's free rule triages
/// alone and nothing is sent to TypeSafe. `TYPESAFE_MODEL` pins the default
/// model (the adapter's pinned default otherwise); a workspace may choose
/// its own. With a key, one call per session sends a redacted digest
/// (counts, the person's and the agent's words cut short, one line per tool
/// call; at most 24k characters) to `api.typesafe.ai` (US).
///
/// Only the compiler (and the settings page's Test) gets it. The Slack
/// judge's `Classifier` port stays unwired (issue #123), so a key here does
/// not change what the Slack agent answers.
struct TypeSafeJev {
    /// The deployment's own key, when the secret is set.
    key: Option<String>,
    /// `TYPESAFE_MODEL`, else the adapter's pinned default.
    model: String,
}

impl TypeSafeJev {
    fn from_env(env: &Env) -> Self {
        let key = env
            .secret("TYPESAFE_API_KEY")
            .ok()
            .map(|secret| secret.to_string())
            .filter(|key| !key.trim().is_empty());
        let model = env
            .var("TYPESAFE_MODEL")
            .ok()
            .map(|model| model.to_string().trim().to_owned())
            .filter(|model| !model.is_empty())
            .unwrap_or_else(|| cratefield_adapter_typesafe::DEFAULT_MODEL.to_owned());
        Self { key, model }
    }
}

impl JevConnector for TypeSafeJev {
    fn deployment_key(&self) -> bool {
        self.key.is_some()
    }

    fn default_model(&self) -> String {
        self.model.clone()
    }

    fn classifier(&self, key: JevKey<'_>, model: &str) -> Option<Arc<dyn Classifier>> {
        let key = match key {
            JevKey::Deployment => self.key.clone()?,
            JevKey::Workspace(key) => key.to_owned(),
        };
        Some(Arc::new(
            TypeSafe::new(Arc::new(FetchClient), Arc::new(WorkersClock), Some(key))
                .with_model(model),
        ))
    }
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

/// The workspaces sign-in mail in this venture's theme: the logo, the brand
/// palette in light and dark, a primary button and the pasteable link — the
/// same shell the waitlist's confirm mails wear, so the first mail a person
/// gets from Living Brain looks like the product rather than a bare page.
///
/// The module renders a plain mail of its own when nothing is injected; this
/// is what the composition injects instead ([`SignInMail`]). The module keeps
/// the per-link idempotency key, so nothing here can change it.
pub struct ThemedSignInMail {
    theme: MailTheme,
}

impl ThemedSignInMail {
    /// The renderer over `theme`; the Worker passes [`mail_theme`].
    #[must_use]
    pub fn new(theme: MailTheme) -> Self {
        Self { theme }
    }
}

impl SignInMail for ThemedSignInMail {
    fn render(&self, input: &SignInMailInput<'_>) -> Rendered {
        if let Some(invitation) = input.invitation {
            let lifetime = input.lifetime();
            // The mail template escapes every text it is handed; the names
            // are the workspace's and a member's own words.
            return MailBody::new(
                "You're invited to Living Brain",
                format!("Join {} on Living Brain", invitation.workspace),
            )
            .preheader(format!(
                "{} invited you. The link works once, for the next {lifetime}.",
                invitation.invited_by
            ))
            .paragraph(format!(
                "{} invited you to the {} workspace on Living Brain.",
                invitation.invited_by, invitation.workspace
            ))
            .button("Accept the invitation", input.link)
            .fallback_link()
            .note(format!("This link works once, for the next {lifetime}."))
            .note("If you weren't expecting it, ignore it — nothing has happened.")
            .recipient(input.recipient)
            .why("a member of a Living Brain workspace invited this address")
            .render(&self.theme)
            .into();
        }
        let minutes = input.minutes;
        MailBody::new("Your Living Brain sign-in link", "Sign in to Living Brain")
            .preheader(format!(
                "Your sign-in link works once, for the next {minutes} minutes."
            ))
            .paragraph("Use the button below to sign in to Living Brain.")
            .button("Sign in", input.link)
            .fallback_link()
            .note(format!(
                "This link works once, for the next {minutes} minutes."
            ))
            .note("If you didn't ask for this, ignore it — nothing has happened.")
            .recipient(input.recipient)
            .why("someone asked to sign in to Living Brain with this address")
            .render(&self.theme)
            .into()
    }
}

/// The sign-in mail renderer the Worker injects into `workspaces`. A function
/// so a native test renders exactly the mail the Worker sends.
#[must_use]
pub fn sign_in_mail() -> Arc<dyn SignInMail> {
    Arc::new(ThemedSignInMail::new(mail_theme()))
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
    jev: Option<Arc<dyn JevConnector>>,
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
    // ADR 0004 "Deleting a workspace": the page store the deletion shreds
    // through, published by the pages closure like `answers` above.
    let erasure = PageStoreCell::default();
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
                    .email_ledger(Arc::new(UsageLedger))
                    // The sign-in mail in the venture's theme, not the
                    // module's plain fallback.
                    .sign_in_mail(sign_in_mail())
                    // ADR 0004: a removed member's tokens are revoked, not
                    // only refused, so re-admitting them revives nothing.
                    .on_member_removed(Arc::new(RevokeOnRemoval))
                    // An owner's deletion shreds the workspace's scopes and
                    // revokes what it holds, through the same page store.
                    .on_workspace_erasure(Arc::new(WipeWorkspace {
                        pages: erasure.clone(),
                    })),
            )
            .module(
                // A Main connection that works resumes the workspace's parked
                // coding-agent backlog (`skipped-no-model`).
                match &jev {
                    Some(jev) => Models::new().with_jev(jev.clone()),
                    None => Models::new(),
                }
                .with_main_connected(Arc::new(ResumeParkedOnMain)),
            )
            .module(Tools::new())
            .module(device_auth_module(&device_ports))
            .module(tokens_module(device_ports))
            // Issue #72: the authorization-code grant a browser MCP client (a
            // Claude connector) signs in through, at `/v1/oauth/*`.
            .module(OAuth::new())
            // `GET /v1/me` (ADR 0004): who a bearer or a session speaks for.
            .module(Me::new())
            // `GET /v1/avatars/{id}`: a member's or the workspace's avatar,
            // to members of that workspace only. The uploads are `me`'s and
            // `workspaces`'; all three are diverted ([`AVATAR_ROUTES`]) so
            // their blob views share one key space.
            .module(Avatars::new())
            // ADR 0004: agent accounts and their keys, access grants (and
            // the tables all three share), and the audit log.
            .module(Access::new())
            .module(Agents::new())
            .module(Audit::new())
            .module(pages_module(kms.clone(), &answers, &erasure))
            .module(waitlist_module())
            // The cost ledger (issue #12): `GET /v1/usage/daily`, backed by
            // the records the models and workspaces modules write through
            // their seams. It needs only what every module here gets — the
            // `DB` binding, the Workers clock and the `HARNESS_SECRET`
            // signer — so the boot gate accepts it where the others stand.
            .module(Usage::new())
            // A workspace's budgets and limits as settings
            // (`GET`/`PATCH /v1/settings/budgets`): the compiler's and the
            // ask route's numbers and the spend limit, over the usage
            // module's `usage_budgets` row. No blob, so it is served like
            // `usage` rather than through the CLI divert.
            .module(BudgetSettings::new());
        // The Discord connection (docs/discord.md): the bot install, its
        // settings routes and the cron drain that imports channel history.
        // Mounted ahead of the CLI modules, so the compiler keeps its place
        // in the cron's split and GitHub stays the last module.
        let builder = builder.module(match kms.clone() {
            Some(kms) => DiscordModule::new().with_kms(kms),
            None => DiscordModule::new(),
        });
        // The CLI modules mount with the same key ring the pages surface gates
        // on, so they join the fold the same way pages does.
        let builder = cli_modules(kms.clone(), jev)
            .into_iter()
            .fold(builder, HarnessBuilder::module_arc);
        // The GitHub connection (docs/github.md): the manifest flow, the
        // install callback, the webhook, the settings routes and the cron
        // drain that backfills into the ledger. Mounted with or without the
        // key ring so its tables are always migrated; without it the flow
        // answers 503 and the drain only does housekeeping. **Mounted
        // last on purpose:** the cron splits one invocation's subrequests
        // across the modules in order, each module's unspent share rolling
        // forward, so the last module inherits whatever the others left.
        let github = match kms {
            Some(kms) => GithubApp::new().with_kms(kms),
            None => GithubApp::new(),
        };
        builder
            .module(github)
            .runtime(runtime.clone())
            .build()
            .expect("the livingbrain harness is valid")
    };
    (harness, runtime, turns)
}

/// The CLI modules (issues #121 and #77, and the temporal facts): the routes `livingbrain`'s
/// client speaks — `POST /v1/notes`, `GET /v1/search`, `POST /v1/ask`,
/// `GET /v1/export`, `GET /v1/sources` (with `/projects` and `/{id}`) —
/// each mounted at its own name under the harness rule, every one
/// constructed with the same [`Wiki`] over the key custodian and the
/// [`TokenBearer`].
///
/// The blob a module's store reads is not decided here — the composition
/// roots the per-request port layer on the pages key space instead, so the
/// page bodies stay physical `pages/` objects no matter which route wrote
/// them ([`PagesRebind`], planted in [`serve_cli`]). With no key custodian
/// there is nothing to seal a page with, so — like [`pages_module`] — none
/// of them is mounted and every other route keeps working.
fn cli_modules(
    kms: Option<Arc<dyn Kms>>,
    jev: Option<Arc<dyn JevConnector>>,
) -> Vec<Arc<dyn Module>> {
    let Some(kms) = kms else {
        return Vec::new();
    };
    // The compiler's triage classifier rides on the same `Wiki`; only the
    // coding-agents compiler asks it.
    let wiki = wiki(Arc::clone(&kms)).with_jev(jev.clone());
    // The three surfaces an actions token may reach (issue #234): each
    // serves exactly one of its four allowed routes, and the writes on them
    // are refused by the token's `read:`-only scopes already.
    let actions = wiki_actions(kms).with_jev(jev);
    vec![
        Arc::new(Notes::new(wiki.clone())),
        Arc::new(Search::new(actions.clone())),
        Arc::new(Ask::new(actions.clone())),
        Arc::new(Export::new(wiki.clone())),
        Arc::new(Facts::new(actions.clone())),
        Arc::new(Sources::new(wiki.clone())),
        // The brain map's data (`GET /v1/graph`): the asker's own pages,
        // sources and facts as nodes and edges, read-only.
        Arc::new(Graph::new(wiki.clone())),
        // Connected apps (issues #51 and #146, ADR 0005): the owner connects
        // an app and issues its credential (`/v1/apps`), publishes pages to
        // customers (`/v1/public`), and the app calls its five tools
        // (`/v1/tools/{name}`, mounted as `app-tools` because the `tools`
        // module owns that name — see [`rewrite_tools_path`]).
        Arc::new(Apps::new(wiki.clone())),
        Arc::new(PublicPages::new(wiki.clone())),
        Arc::new(AppTools::new(wiki.clone())),
        // (Mounted ahead of the compiler: it must stay third from last for the
        // cron's split, and these modules have no scheduled work.)
        // Dreaming's settings and its decision log (`/v1/dreaming`): the
        // workspace's switch, model and "Run now", over the same table the
        // cron reads. Mounted before the compiler, which keeps the compiler's
        // cron share where it was.
        Arc::new(Dreaming::new(wiki.clone())),
        // The coding-agents compiler: its queue and status routes, and the
        // cron drain that compiles synced sessions into `coding-agents/`.
        Arc::new(CodingAgents::new(wiki.clone())),
        // The local runner's job handoff: a member's own device claims the
        // model calls of their jobs and hands the answers back.
        Arc::new(ModelJobs::new(wiki)),
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

/// What removing a member does beyond the workspaces module's own tombstone
/// (ADR 0004): every personal access token they hold is revoked, every agent
/// account they own is disabled and its keys revoked (an admin can hand it to
/// somebody else and re-enable it), every grant naming them is deleted, and
/// every grant on their personal space too — so nobody keeps a share of a
/// space whose owner has gone. The tombstone already refuses their own
/// credentials; this is what keeps a re-admitted id from finding them again.
///
/// Best effort, per the seam's contract: the removal has happened. A failure
/// here leaves the agents' keys refused anyway, because `authenticate_any`
/// requires the owner to be a live member.
struct RevokeOnRemoval;

#[async_trait::async_trait]
impl MemberRemoval for RevokeOnRemoval {
    async fn removed(
        &self,
        db: &dyn Database,
        now: OffsetDateTime,
        workspace_id: &str,
        user_id: &str,
    ) {
        let stamp = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        let _ = livingbrain_tokens::revoke_member_tokens(db, workspace_id, user_id, &stamp).await;
        let _ = cratefield_module_orgs::service_accounts::disable_owned_by(
            db,
            workspace_id,
            user_id,
            now,
        )
        .await;
        let _ = cratefield_module_orgs::grants::delete_grants_for(
            db,
            workspace_id,
            &livingbrain_pages::access::member_principal(user_id),
        )
        .await;
        let _ = cratefield_module_orgs::grants::delete_grants_on_space(
            db,
            workspace_id,
            &livingbrain_pages::access::personal(user_id),
        )
        .await;
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
fn pages_module(
    kms: Option<Arc<dyn Kms>>,
    answers: &AnswersCell,
    erasure: &PageStoreCell,
) -> Pages {
    let Some(kms) = kms else {
        return Pages::new();
    };
    // Each surface gets its own handle to the same custodian and the same
    // credential resolver: one key, so a body either of them seals is a body
    // the other can open.
    let auth: Arc<dyn BearerAuth> = Arc::new(TokenBearer);
    let mcp_kms = Arc::clone(&kms);
    // The MCP endpoint alone also takes a connector token (issue #72); every
    // other bearer surface refuses one, as `TokenBearer` does.
    let mcp_auth = mcp_bearer();
    // The page routes the web app reads and writes answer to its session
    // cookie as well as to a bearer; the MCP endpoint below takes the bearer
    // resolver alone and stays bearer-only, and the importer takes the
    // cookie too so the Imports page can upload (same origin only). The
    // surface also answers an actions token on `GET /{slug}` alone, and
    // [`pages_surface`]'s guard holds it to that one route.
    let surface = wiki_actions(Arc::clone(&kms));
    let cell = answers.clone();
    let shred = erasure.clone();
    Pages::new()
        .surface(move |ctx| pages_surface(ctx, &surface))
        .nest("/mcp", move |ctx| {
            // Read apart from `ctx`, which the MCP router below takes whole.
            if let (Some(db), Some(blob), Some(clock), Some(id_gen)) = (
                ctx.ports.db.clone(),
                ctx.ports.blob.clone(),
                ctx.ports.clock.clone(),
                ctx.ports.id_gen.clone(),
            ) {
                shred.publish(Arc::new(PageStore::new(
                    db.clone(),
                    blob.clone(),
                    Arc::clone(&mcp_kms),
                    clock.clone(),
                    id_gen.clone(),
                )));
                cell.publish(Arc::new(PageAnswers::new(
                    PageStore::new(db, blob, Arc::clone(&mcp_kms), clock, id_gen),
                    // Where a citation opens: this deployment's own web app
                    // (`WORKSPACES_REDIRECT_BASE`), the origin every other
                    // surface cites too — not the marketing site.
                    livingbrain_pages::app_base(ctx.config.as_ref()),
                )));
            }
            livingbrain_mcp::router(ctx, Arc::clone(&mcp_kms), Arc::clone(&mcp_auth))
        })
        .nest("/sources", move |ctx| {
            livingbrain_mcp::sources_router_with_session(
                ctx,
                Arc::clone(&kms),
                Arc::clone(&auth),
                Arc::new(WebSession),
            )
        })
}

/// The page store the workspace deletion shreds through (ADR 0004), in the
/// same shape as [`AnswersCell`]: the pages module's own context builds it
/// per request, because a module's blob is scoped to that module's name and
/// only the pages surface can open the bodies its pages wrote.
///
/// Empty until a request runs the pages closure. Erasure reads an empty slot
/// as a failure and refuses the deletion: nothing is deleted that cannot be
/// shredded.
#[derive(Clone, Default)]
struct PageStoreCell(Arc<std::sync::RwLock<Option<Arc<PageStore>>>>);

impl PageStoreCell {
    fn publish(&self, store: Arc<PageStore>) {
        if let Ok(mut slot) = self.0.write() {
            *slot = Some(store);
        }
    }

    fn get(&self) -> Option<Arc<PageStore>> {
        self.0.read().ok().and_then(|slot| slot.clone())
    }
}

/// What deleting a workspace does beyond the workspaces module's own rows
/// (ADR 0004, "Deleting a workspace"): every content scope the workspace
/// owns is crypto-shredded through [`PageStore::forget_scope`], and every
/// credential other modules hold for it is revoked or deleted.
///
/// Every step is idempotent, so a retry after a failure reruns the lot. The
/// credential statements are one batch; the shreds are one per scope, each
/// atomic, which is as atomic as the store allows across scopes.
struct WipeWorkspace {
    pages: PageStoreCell,
}

#[async_trait::async_trait]
impl WorkspaceErasure for WipeWorkspace {
    async fn erase(
        &self,
        db: &dyn Database,
        now: OffsetDateTime,
        workspace_id: &str,
        spaces: &[String],
    ) -> Result<(), ErasureFailed> {
        let store = self.pages.get().ok_or(ErasureFailed)?;
        // The scopes of connected apps (issues #51 and #146): every subject
        // an app keeps memories for, and the workspace's public scope. They
        // are named by no member, so they are found by their own rows —
        // before the shreds below delete those rows.
        let mut app_scopes = livingbrain_pages::app_scopes(db, workspace_id)
            .await
            .map_err(|_| ErasureFailed)?;
        // The public scope always: a withdrawn page's copy is still sealed in
        // it, with no ledger row left to name it.
        app_scopes.push(livingbrain_pages::page_scope(
            workspace_id,
            &livingbrain_access::Scope::Public,
        ));
        for scope in &app_scopes {
            store.forget_scope(scope).await.map_err(|_| ErasureFailed)?;
        }
        for space in spaces {
            let scope =
                livingbrain_pages::access::space_scope(workspace_id, space).ok_or(ErasureFailed)?;
            store
                .forget_scope(&scope)
                .await
                .map_err(|_| ErasureFailed)?;
        }
        let stamp = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default();
        revoke_workspace_credentials(db, workspace_id, &stamp)
            .await
            .map_err(|_| ErasureFailed)
    }
}

/// The credentials and access a workspace holds in the tables other modules
/// own, in one batch (ADR 0004): personal access tokens and agent keys are
/// revoked, agent accounts disabled, grants deleted; the workspace's provider
/// keys, tool credentials, GitHub App key and installations are deleted, as
/// is its Slack bot token copy (in the workspaces module's own batch).
///
/// Audit trails (`org_audit`, `settings_audit`) and usage counts are kept:
/// they name no content, and ADR 0004 keeps the audit log on purpose.
async fn revoke_workspace_credentials(
    db: &dyn Database,
    workspace_id: &str,
    stamp: &str,
) -> Result<(), cratefield_core::DbError> {
    db.batch_atomic(&[
        cratefield_core::Statement::with_values(
            "UPDATE personal_access_tokens SET revoked_at = ? \
              WHERE workspace_id = ? AND revoked_at IS NULL",
            vec![stamp.into(), workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "UPDATE org_service_account_keys SET revoked_at = ? \
              WHERE org_id = ? AND revoked_at IS NULL",
            vec![stamp.into(), workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "UPDATE org_service_accounts SET disabled_at = ? \
              WHERE org_id = ? AND disabled_at IS NULL",
            vec![stamp.into(), workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM org_grants WHERE org_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM model_connections WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM tool_connections WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM tool_workspace_settings WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM tool_pending_approvals WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM github_repos WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM github_installations WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM github_states WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
        cratefield_core::Statement::with_values(
            "DELETE FROM github_app WHERE workspace_id = ?",
            vec![workspace_id.into()],
        ),
    ])
    .await
    .map(|_| ())
}

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

/// The [`Wiki`] every route of `livingbrain-api` is built with: the key
/// custodian, the [`TokenBearer`] for the CLI's personal access token, and
/// the [`WebSession`] for the web app's session cookie. One function, so the
/// pages surface and the CLI modules answer to exactly the same
/// credentials — and so a native test mounts what the Worker serves.
#[must_use]
pub fn wiki(kms: Arc<dyn Kms>) -> Wiki {
    let session: Arc<dyn SessionAuth> = Arc::new(WebSession);
    Wiki::new(kms, Arc::new(TokenBearer)).with_session(session)
}

/// The [`Wiki`] the four read-only routes an actions token may call are
/// built with (issue #234, a ChatGPT custom GPT's Actions): `search`, `ask`
/// and `facts` in [`cli_modules`], and the pages surface in
/// [`pages_module`]. Exactly [`wiki`], but the bearer also admits an actions
/// token ([`livingbrain_tokens::ACTIONS_AUDIENCE`]) beside the ordinary
/// credentials. The token is read-only by construction — its scope subset
/// is `read:`-prefixed and non-empty at the mint — and the pages surface
/// wraps [`actions_guard`] around it, which refuses the token on every
/// route of that surface but `GET /v1/pages/{slug}`. Every other module
/// keeps [`wiki`], whose bearer answers an actions token as an unknown one.
#[must_use]
pub fn wiki_actions(kms: Arc<dyn Kms>) -> Wiki {
    let session: Arc<dyn SessionAuth> = Arc::new(WebSession);
    Wiki::new(kms, Arc::new(ActionsBearer)).with_session(session)
}

/// The [`BearerAuth`] the MCP endpoint below `/v1/pages` resolves with:
/// [`TokenBearer`]'s credentials plus a connector token — the one audience
/// no other bearer surface takes. Public so a native test mounts what the
/// Worker serves, the same reason [`wiki`] is.
#[must_use]
pub fn mcp_bearer() -> Arc<dyn BearerAuth> {
    Arc::new(McpBearer)
}

/// The [`SessionAuth`] behind the web app's calls to `livingbrain-api`: the
/// session cookie, resolved by the tokens module's [`livingbrain_tokens::session`]
/// — the cookie half of the one `authenticate` every cookie route here
/// answers to (`/v1/models`, `/v1/usage`, `/v1/tokens`), so a browser is
/// the same member on the wiki as on the settings page. It never reads a
/// bearer: `livingbrain-api` only consults it for a request with no
/// `Authorization` header, and a token goes through [`TokenBearer`].
///
/// The same-origin rule is the tokens module's too
/// ([`livingbrain_tokens::same_origin`]), so a cookie write is held to one
/// rule wherever it lands.
struct WebSession;

#[async_trait::async_trait]
impl SessionAuth for WebSession {
    async fn session(&self, ports: &Ports, headers: &HeaderMap) -> Result<Asker, AuthError> {
        livingbrain_tokens::session(ports, headers)
            .await
            .map(member_asker)
            .map_err(|_| AuthError::Rejected)
    }

    fn same_origin(&self, headers: &HeaderMap) -> Result<(), String> {
        livingbrain_tokens::same_origin(headers).map_err(|refused| refused.detail)
    }
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
        // An agent account's key is accepted here and only here (ADR 0004):
        // these are the routes whose every read and write goes through the
        // grants plan, where an agent reaches exactly what it was granted.
        livingbrain_tokens::authenticate_any(ports, &headers)
            .await
            .map(asker_of)
            .map_err(|_| AuthError::Rejected)
    }
}

/// The [`BearerAuth`] for the MCP endpoint alone (issue #72). It resolves the
/// same credentials [`TokenBearer`] does, and additionally a connector token,
/// the one kind of token no other surface accepts. The connector token is
/// non-admin by construction (its scope subset), so the MCP tools see exactly
/// what that subset grants.
struct McpBearer;

#[async_trait::async_trait]
impl BearerAuth for McpBearer {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        if !token.bytes().all(is_cookie_octet) {
            return Err(AuthError::Rejected);
        }
        let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) else {
            return Err(AuthError::Rejected);
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);
        livingbrain_tokens::authenticate_mcp(ports, &headers)
            .await
            .map(asker_of)
            .map_err(|_| AuthError::Rejected)
    }
}

/// The [`BearerAuth`] for the four read-only REST routes an actions token
/// may call (issue #234, a ChatGPT custom GPT's Actions). It resolves the
/// same credentials [`TokenBearer`] does, and additionally an actions token
/// — the one audience no other REST surface takes. That token is read-only
/// and never admin by construction (its scope subset is `read:`-prefixed
/// and non-empty at the mint), so these routes see exactly what the subset
/// grants, and no write among them opens to it.
struct ActionsBearer;

#[async_trait::async_trait]
impl BearerAuth for ActionsBearer {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        if !token.bytes().all(is_cookie_octet) {
            return Err(AuthError::Rejected);
        }
        let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}")) else {
            return Err(AuthError::Rejected);
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);
        livingbrain_tokens::authenticate_actions(ports, &headers)
            .await
            .map(asker_of)
            .map_err(|_| AuthError::Rejected)
    }
}

/// The one allowlist of what an actions token may call (issue #234): the
/// four read-only REST endpoints a ChatGPT custom GPT's Actions are pointed
/// at — `GET /v1/search`, `GET /v1/pages/{slug}` (one page, never the list
/// and never `mcp`), `GET /v1/facts` and `POST /v1/ask`.
///
/// The [`BearerAuth`] seam cannot see a request — its `authenticate` takes
/// a token alone — so the allowlist is enforced where one can:
/// [`actions_guard`] wraps the pages surface, the one module whose routes
/// mix an allowed read (`GET /{slug}`) with refused ones (the list, `PUT`,
/// and the `mcp` nest beside it); the other three modules each serve
/// exactly one allowed route, with writes among them already refused by the
/// token's `read:`-only scopes. `path` may be the request's whole path or
/// the prefix-stripped view a nested router sees; both spellings answer
/// the same.
fn actions_request_allowed(method: &HttpMethod, path: &str) -> bool {
    let path = path.split('?').next().unwrap_or(path);
    let read = method == HttpMethod::GET || method == HttpMethod::HEAD;
    if read && (path == "/v1/search" || path == "/v1/facts") {
        return true;
    }
    if method == HttpMethod::POST && path == "/v1/ask" {
        return true;
    }
    if !read {
        return false;
    }
    // `GET /v1/pages/{slug}`: exactly one segment after the mount, which
    // the surface's own router spells `/{slug}` once the nest has stripped
    // its prefix — and never `mcp`, the MCP endpoint nested beside it.
    path.strip_prefix("/v1/pages/")
        .or_else(|| path.strip_prefix('/'))
        .is_some_and(|slug| !slug.is_empty() && !slug.contains('/') && slug != "mcp")
}

/// The route-aware half of an actions token's restriction, wrapped around
/// the pages surface by [`pages_surface`]: a bearer whose audience is
/// `actions` passes only where [`actions_request_allowed`] says, answered
/// with the same 401 every refused credential gets. Every other request
/// passes through untouched — the allowlisted routes cost nothing, and a
/// request with no bearer, or one the tokens module does not know, keeps
/// the surface's own answer.
async fn actions_guard(
    AxumState(ctx): AxumState<Arc<ModuleContext>>,
    req: HttpRequest<Body>,
    next: Next,
) -> AxumResponse {
    if actions_request_allowed(req.method(), req.uri().path()) {
        return next.run(req).await;
    }
    if let Some(token) = cratefield_core::bearer_token(req.headers())
        && livingbrain_tokens::bearer_audience(&ctx.ports, token)
            .await
            .as_deref()
            == Some(livingbrain_tokens::ACTIONS_AUDIENCE)
    {
        return livingbrain_tokens::AuthError::problem().into_response();
    }
    next.run(req).await
}

/// The pages surface as the Worker serves it: the page routes over the same
/// [`Wiki`] as every other surface, wrapped in [`actions_guard`] — the one
/// place a route, not just a credential, decides what an actions token
/// reaches. Public so a native test mounts what the Worker mounts, the same
/// reason [`wiki`] is.
pub fn pages_surface(ctx: Arc<ModuleContext>, wiki: &Wiki) -> Router {
    livingbrain_api::pages_routes(Arc::clone(&ctx), wiki)
        .layer(from_fn_with_state(ctx, actions_guard))
}

/// A member as the wiki routes see them: workspace, id, role, and the
/// credential's scope subset and prefix (a session carries neither).
fn member_asker(member: livingbrain_tokens::Member) -> Asker {
    Asker {
        workspace_id: member.workspace_id,
        user_id: member.user_id,
        token_scopes: member.scopes,
        agent_owner: None,
        role: Some(member.role.as_str().to_owned()),
        credential: member.credential,
    }
}

/// A tokens-module resolution as the wiki routes see it: a member or an
/// agent account, the one shape every bearer below answers with.
fn asker_of(authenticated: Authenticated) -> Asker {
    match authenticated {
        Authenticated::Member(member) => member_asker(member),
        Authenticated::Agent(agent) => Asker {
            workspace_id: agent.workspace_id,
            user_id: agent.agent_id,
            token_scopes: agent.scopes,
            agent_owner: Some(agent.owner_id),
            role: None,
            credential: Some(agent.credential),
        },
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
            Some(Arc::new(TypeSafeJev::from_env(env))),
        )
    })
}

/// The CLI paths `fetch` diverts to [`serve_cli`] — the routes the
/// [`cli_modules`] mount, minus `/v1/pages`, which the pages module
/// serves through the ordinary [`serve`] path like every other route.
const CLI_ROUTES: [&str; 13] = [
    "/v1/notes",
    "/v1/search",
    "/v1/graph",
    "/v1/ask",
    "/v1/export",
    "/v1/sources",
    "/v1/coding-agents",
    "/v1/facts",
    "/v1/model-jobs",
    "/v1/dreaming",
    "/v1/apps",
    "/v1/public",
    "/v1/tools",
];

/// The diverted paths that have routes *under* them, so the divert matches
/// the prefix: the sources listing, facet and citation route, the
/// coding-agents queue and process routes, and a fact's retraction
/// (`/v1/facts/{id}/retract`).
const CLI_PREFIXES: [&str; 8] = [
    "/v1/sources",
    "/v1/coding-agents",
    "/v1/facts",
    "/v1/model-jobs",
    "/v1/dreaming",
    "/v1/apps",
    "/v1/public",
    "/v1/tools",
];

/// The path the app tools are mounted at (the module is `app-tools`) and the
/// public path they are served under. The harness mounts a module at
/// `/v1/<name>` and `tools` is taken (the MCP-client module, which has no
/// routes), while the contract clients speak is `POST /v1/tools/<name>`;
/// `serve_cli` re-addresses the one to the other before the router sees it.
const APP_TOOLS_MOUNT: &str = "/v1/app-tools";
const APP_TOOLS_PUBLIC: &str = "/v1/tools";

/// The path a diverted request is routed under: `/v1/tools/<name>` becomes
/// `/v1/app-tools/<name>`, anything else is unchanged. Only `/v1/tools` and
/// what is below it moves, so `/v1/toolsx` is nobody's route.
fn rewrite_tools_path(path: &str) -> std::borrow::Cow<'_, str> {
    match path.strip_prefix(APP_TOOLS_PUBLIC) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            std::borrow::Cow::Owned(format!("{APP_TOOLS_MOUNT}{rest}"))
        }
        _ => std::borrow::Cow::Borrowed(path),
    }
}

/// The avatar paths `fetch` diverts to [`serve_cli`]: the two uploads and
/// the read route. Three modules touch one avatar key space (`me`,
/// `workspaces`, `avatars`), and the harness scopes each module's blob view
/// under its own name, so — exactly like the CLI modules and the pages key
/// space — they are served through the stack that plants an
/// [`AvatarRebind`] under the views. The uploads match exactly; the read
/// route by prefix (`/v1/avatars/{id}`).
const AVATAR_ROUTES: [&str; 2] = ["/v1/me/avatar", "/v1/workspaces/current/avatar"];

/// The prefix of the avatar read route.
const AVATAR_PREFIX: &str = "/v1/avatars/";

/// Whether this request is one of the avatar paths. Unconditional: the
/// avatar modules mount with or without a key ring.
fn serves_avatars(path: &str) -> bool {
    AVATAR_ROUTES.contains(&path)
        || path
            .strip_prefix(AVATAR_PREFIX)
            .is_some_and(|id| !id.is_empty() && !id.contains('/'))
}

/// The `Cache-Control` an avatar read carries instead of the harness's
/// `no-store`, or `None` for every other request.
///
/// The harness stamps `no-store` on every `/v1` answer, which is right for
/// JSON and wrong for an image whose URL changes on every upload: each page
/// load would fetch every avatar again. A `GET` of `/v1/avatars/{id}` that
/// succeeded is the only answer relaxed, and only to a **private** cache —
/// the image is member-only, and `Vary: Cookie, Authorization` rides along.
fn avatar_cache_control(method: &str, path: &str) -> Option<&'static str> {
    let read = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
    (read && path.starts_with(AVATAR_PREFIX) && serves_avatars(path))
        .then_some(livingbrain_workspaces::AVATAR_CACHE_CONTROL)
}

/// Whether this request is one of the CLI paths the venture diverts,
/// and the surface is mounted at all. The mount is gated on the key ring
/// (see [`cli_modules`]), so the presence of the `notes` module is the same
/// gate — a deployment without the ring takes these paths through the
/// ordinary `serve`, which answers 404, exactly as a composition without
/// the modules should.
///
/// Most are single endpoints and match exactly — a subroute
/// under them is nobody's route, and exact matching keeps a future one from
/// silently changing handler stack. The sources module is the first
/// exception: the bare `/v1/sources` is its listing, `/v1/sources/projects`
/// its projects facet, and `/v1/sources/{id}` the citation route, so the
/// prefix diverts — matching only the bare path would drop every real
/// citation on the floor. The coding-agents queue and a fact's retraction
/// (`/v1/facts/{id}/retract`) divert by prefix too ([`CLI_PREFIXES`]).
fn serves_cli(harness: &Harness, path: &str) -> bool {
    let diverted = CLI_ROUTES.iter().any(|route| {
        path == *route
            || (CLI_PREFIXES.contains(route)
                && path
                    .strip_prefix(route)
                    .is_some_and(|rest| rest.starts_with('/')))
    });
    diverted && harness.modules().iter().any(|m| m.name() == "notes")
}

/// The device grant's approval page (issue #72): `GET /v1/device-auth`, the
/// one `/v1` document whose form posts back to `/v1`.
const DEVICE_PAGE: &str = "/v1/device-auth";

/// The `Referrer-Policy` the approval page must carry instead of the
/// harness's `no-referrer`, or `None` for every other response.
///
/// **Why.** The harness stamps `Referrer-Policy: no-referrer` on every
/// `/v1/*` response (`cratefield-core` 0.9.0, `security_headers_layer` in
/// `src/http.rs`). That is right for JSON, but the device grant's page
/// (`cratefield-module-device-auth` 0.3.0, `approval_page` in
/// `src/handlers.rs`) is a document with two `<form method="post">`
/// buttons, and the Fetch standard's "append a request `Origin` header"
/// step serializes the origin of a non-`GET` request as `null` when the
/// request's referrer policy is `no-referrer` — and a form submission
/// inherits that policy from the page that holds the form. So every press of
/// Approve or Deny reached `/v1/device-auth/approve` with `Origin: null`,
/// and the module's `require_same_origin` refuses exactly that ("origin is
/// null; requests must come from this venture's own origin", 403). The
/// module has no allow-list or switch for it, and it is right not to: a
/// `null` origin is also what a sandboxed cross-site frame sends.
///
/// **The policy.** `same-origin` sends the real origin (and the referrer) to
/// this venture's own origin and nothing at all to anyone else, so the
/// approve POST carries `Origin: https://<this host>` and passes the check,
/// while the page's URL — which holds the `user_code` — still never reaches
/// another site. It is the narrowest policy under which the form works.
///
/// **Scope.** Only `GET`/`HEAD` of the page itself, whatever it answers:
/// the code form, the unknown-code page, the approval page, or the `303` to
/// sign-in (a redirect's policy governs the next hop, which is this
/// venture's own sign-in page). The decision `POST`s answer documents with
/// no form and keep `no-referrer`, and so does every other `/v1` route —
/// the workspaces email-verify page in particular, whose URL carries a live
/// sign-in token and which sets `no-referrer` itself.
fn page_referrer_policy(method: &str, path: &str) -> Option<&'static str> {
    let is_page = path == DEVICE_PAGE || path.strip_suffix('/') == Some(DEVICE_PAGE);
    let is_read = method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD");
    (is_page && is_read).then_some("same-origin")
}

/// The connector token endpoint (issue #72), and the device grant's poll path.
const OAUTH_TOKEN_PATH: &str = "/v1/oauth/token";
const DEVICE_TOKEN_PATH: &str = "/v1/device-auth/token";

/// Whether a token request's form body names the device grant.
fn is_device_poll(body: &str) -> bool {
    body.split('&').any(|pair| {
        matches!(
            pair,
            "grant_type=urn:ietf:params:oauth:grant-type:device_code"
                | "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        )
    })
}

/// One token endpoint for every grant a client may use (RFC 8628 §3.4 and
/// issue #72): a device-grant poll sent to `/v1/oauth/token` is re-addressed to
/// the device grant's own token path, the same request otherwise. The device
/// handler is mounted by the harness and cannot be reached from the OAuth
/// module, so the Worker does the forwarding; the body is read once and sent
/// on unchanged.
async fn route_token_request(mut req: Request) -> worker::Result<Request> {
    if req.method().as_ref() != "POST" || req.path() != OAUTH_TOKEN_PATH {
        return Ok(req);
    }
    let body = req.text().await?;
    let target = if is_device_poll(&body) {
        DEVICE_TOKEN_PATH
    } else {
        OAUTH_TOKEN_PATH
    };
    let mut url = req.url()?;
    url.set_path(target);
    let mut init = worker::RequestInit::new();
    init.with_method(req.method())
        .with_headers(req.headers().clone());
    init.body = Some(worker::wasm_bindgen::JsValue::from_str(&body));
    Request::new_with_init(url.as_str(), &init)
}

/// Worker fetch entry point.
///
/// # Errors
///
/// Propagates `worker::Error` from the harness router.
#[event(fetch)]
pub async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let req = route_token_request(req).await?;
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
    if serves_avatars(req.path().as_str()) {
        let cache = avatar_cache_control(req.method().as_ref(), &req.path());
        let mut response = serve_cli(harness, runtime, req, env, ctx).await?;
        if let Some(policy) = cache.filter(|_| response.status_code() == 200) {
            // `set` replaces the harness's `no-store`; a failure leaves the
            // stricter header, which costs a refetch and nothing else.
            let _ = response.headers_mut().set("cache-control", policy);
        }
        return Ok(response);
    }
    if serves_cli(harness, req.path().as_str()) {
        return serve_cli(harness, runtime, req, env, ctx).await;
    }
    // Read before `serve` takes the request; see `page_referrer_policy`.
    let referrer_policy = page_referrer_policy(req.method().as_ref(), &req.path());
    let device_document = device_pages::is_document(req.method().as_ref(), &req.path());
    let mut response = serve(harness, runtime, req, env, ctx).await?;
    if device_document {
        response = brand_device_page(response).await?;
    }
    if let Some(policy) = referrer_policy {
        // `set` replaces the harness's `no-referrer` rather than adding a
        // second value. A failure leaves the stricter header standing, which
        // breaks the button but never weakens anything, so it is not worth
        // failing the page over.
        let _ = response.headers_mut().set("referrer-policy", policy);
    }
    Ok(response)
}

/// Re-renders a device-grant HTML answer in the web app's shell; see
/// [`device_pages`] for why the venture does this rather than the module.
///
/// Only an HTML body is touched: the module's problem+json refusals and the
/// `303` to sign-in pass through as they are. The status and every header
/// the harness stamped are carried over unchanged — `no-store`, `nosniff`,
/// `X-Frame-Options`, `frame-ancestors 'none'` — except `content-length`,
/// which described the old body, and the branded page's own CSP is
/// *appended*, so it can only narrow what the harness allowed.
async fn brand_device_page(mut response: Response) -> worker::Result<Response> {
    let content_type = response.headers().get("content-type").ok().flatten();
    if !device_pages::is_html(content_type.as_deref()) {
        return Ok(response);
    }
    let status = response.status_code();
    let headers = response.headers().clone();
    let body = response.text().await?;
    let _ = headers.delete("content-length");
    let _ = headers.append("content-security-policy", device_pages::PAGE_CSP);
    Ok(Response::from_html(device_pages::brand(&body))?
        .with_status(status)
        .with_headers(headers))
}

/// Serves one of the CLI paths ([`CLI_ROUTES`]) through the harness
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
/// `!Send` bridge and a hand-rolled cap), the CLI routes ask for a
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
    // The avatar paths' three views land on one `avatars/…` space the same
    // way ([`AVATAR_ROUTES`]); its keys and the pages module's are disjoint,
    // so the two rebinds stack in either order.
    ports.blob = ports
        .blob
        .take()
        .map(|raw| Arc::new(PagesRebind::new(Arc::new(AvatarRebind::new(raw)))) as Arc<dyn Blob>);
    // `ports` moves into `router()` below, and the ceiling lookup reads the
    // same config the module contexts were built with — clone the `Arc`
    // out first, the way `serve` does.
    let config = Arc::clone(&ports.config);
    let router = harness.router(ports);
    let problem_type_base = harness.venture().problem_type_base();

    let mut url = req.url()?;
    // The app tools are served at `/v1/tools/…` but mounted as `app-tools`.
    let routed = rewrite_tools_path(url.path()).into_owned();
    url.set_path(&routed);
    let path = routed;
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

/// The most an ordinary (buffered) CLI response body may be: 1 MiB, a
/// generous ceiling for JSON and one zip. A streamed body is not held to it.
const MAX_RESPONSE_BUFFER: usize = 1024 * 1024;

/// What the Worker bridge sends for a response body.
enum BridgeBody {
    /// The chunks of a [`ResponseStream`], passed through as they come.
    Streamed(BoxStream<'static, Result<Bytes, StreamError>>),
    /// An ordinary body, read whole under [`MAX_RESPONSE_BUFFER`].
    Buffered(Bytes),
}

/// Takes a response body for the Worker bridge, the way the runtime's own
/// bridge does: a [`ResponseStream`] hands over its stream unbuffered, and
/// any other body is buffered under the ceiling. A large map answered as a
/// streamed body would otherwise fail at the ceiling with a bare
/// `length limit exceeded`.
async fn bridge_body(
    extensions: &Extensions,
    body: Body,
) -> Result<BridgeBody, cratefield_core::axum::Error> {
    if let Some(stream) = extensions
        .get::<ResponseStream>()
        .and_then(ResponseStream::take)
    {
        return Ok(BridgeBody::Streamed(stream));
    }
    to_bytes(body, MAX_RESPONSE_BUFFER)
        .await
        .map(BridgeBody::Buffered)
}

/// The response bridge: a streamed body is carried chunk by chunk, an
/// ordinary one buffered (see [`bridge_body`]).
async fn response_to_worker(response: AxumResponse) -> worker::Result<Response> {
    let (parts, body) = response.into_parts();
    let bridged = bridge_body(&parts.extensions, body)
        .await
        .map_err(|err| worker::Error::RustError(err.to_string()))?;
    let mut out = match bridged {
        BridgeBody::Streamed(stream) => Response::from_stream(stream.map(|chunk| {
            chunk
                .map(Vec::from)
                .map_err(|err| worker::Error::RustError(err.to_string()))
        }))?,
        BridgeBody::Buffered(bytes) => Response::from_bytes(bytes.to_vec())?,
    }
    .with_status(parts.status.as_u16());
    copy_headers(out.headers_mut(), &parts.headers);
    Ok(out)
}

#[cfg(test)]
mod bridge_tests {
    use super::*;
    use std::convert::Infallible;

    const TWO_MIB: usize = 2 * 1024 * 1024;

    async fn drain(body: BridgeBody) -> Vec<u8> {
        match body {
            BridgeBody::Streamed(stream) => {
                let chunks: Vec<Result<Bytes, StreamError>> = stream.collect().await;
                chunks
                    .into_iter()
                    .flat_map(|chunk| chunk.expect("a clean chunk").to_vec())
                    .collect()
            }
            BridgeBody::Buffered(bytes) => bytes.to_vec(),
        }
    }

    #[test]
    fn a_streamed_body_past_the_buffer_ceiling_is_carried_whole() {
        let chunks = (0..TWO_MIB).step_by(64 * 1024).map(|start| {
            Ok::<Bytes, Infallible>(Bytes::from(vec![7u8; (TWO_MIB - start).min(64 * 1024)]))
        });
        let response = ResponseStream::new(futures_util::stream::iter(chunks)).into_response();
        let (parts, body) = response.into_parts();
        let bridged = pollster::block_on(bridge_body(&parts.extensions, body))
            .expect("a streamed body is not read under the ceiling");
        let bytes = pollster::block_on(drain(bridged));
        assert_eq!(bytes.len(), TWO_MIB);
    }

    #[test]
    fn an_ordinary_body_past_the_buffer_ceiling_is_refused_as_before() {
        let response = AxumResponse::new(Body::from(vec![1u8; TWO_MIB]));
        let (parts, body) = response.into_parts();
        let refused = pollster::block_on(bridge_body(&parts.extensions, body));
        let err = refused.err().expect("an over-ceiling buffer is refused");
        assert_eq!(err.to_string(), "length limit exceeded");
    }

    #[test]
    fn an_ordinary_small_body_is_buffered_intact() {
        let response = AxumResponse::new(Body::from(b"{\"ok\":true}".to_vec()));
        let (parts, body) = response.into_parts();
        let bridged = pollster::block_on(bridge_body(&parts.extensions, body)).expect("buffered");
        assert_eq!(pollster::block_on(drain(bridged)), b"{\"ok\":true}");
    }
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
/// confirmation claims here, and the `coding-agents` module drains its
/// compile queue. The cron lives in `wrangler.toml`; a trigger on another
/// Worker never reaches this handler.
///
/// **Why not `serve_scheduled`.** It derives the ports per invocation and
/// hands each module its own scoped view, which is exactly right — except
/// that the coding-agents compiler, like the other CLI modules, reads and
/// writes page bodies, and a scoped view under its own name is a key space
/// no page lives in. So this is `serve_scheduled_with_limits`' loop with
/// one change, the one [`serve_cli`] makes per request: the raw store is
/// wrapped in [`PagesRebind`] before the views are cut, so the sibling
/// modules' keys land on `pages/…` and every other key passes through
/// unchanged. The split and the error handling are the runtime's own
/// (`cratefield-runtime-cloudflare` 0.4.3, `src/lib.rs`); the limits are
/// this venture's, sized for Workers Paid ([`scheduled_limits`]), and the
/// compile drain's deadline is held under Cloudflare's wall
/// ([`module_budget`]).
pub async fn scheduled(event: worker::ScheduledEvent, env: Env, ctx: worker::ScheduleContext) {
    let (harness, runtime, _) = instance(&env);
    install_tracing();
    let cron = event.cron();
    let mut ports = runtime.ports(&env, Arc::new(ScheduleDefer(ctx)));
    ports.blob = ports
        .blob
        .take()
        .map(|raw| Arc::new(PagesRebind::new(raw)) as Arc<dyn Blob>);
    let clock = ports.clock.clone();
    let start = clock
        .as_ref()
        .map_or(OffsetDateTime::UNIX_EPOCH, |clock| clock.now());
    let limits = scheduled_limits(
        env.var("SCHEDULED_WALL_SECS")
            .ok()
            .map(|v| v.to_string())
            .as_deref(),
        env.var("SCHEDULED_SUBREQUESTS")
            .ok()
            .map(|v| v.to_string())
            .as_deref(),
    );
    let mut split = ScheduledSplit::new(limits, start, harness.modules().len());
    let mut took = Vec::new();
    for module in harness.modules() {
        let now = clock
            .as_ref()
            .map_or(OffsetDateTime::UNIX_EPOCH, |clock| clock.now());
        let budget = Arc::new(module_budget(module.name(), split.next(now), start));
        let mut module_ctx = harness.module_context(module.as_ref(), &ports);
        module_ctx.scheduled = Arc::clone(&budget);
        if let Err(err) = module.scheduled(&module_ctx, &cron).await {
            // Logged, never a failed cron — the runtime's own rule.
            worker::console_error!(
                "scheduled module work failed: module={} cron={} error={}",
                module.name(),
                cron,
                err
            );
        }
        split.settle(&budget);
        let end = clock.as_ref().map_or(now, |clock| clock.now());
        took.push((module.name().to_owned(), end - now));
    }
    let end = clock.as_ref().map_or(start, |clock| clock.now());
    worker::console_log!("{}", tick_line(&cron, end - start, &took));
}

/// The one line a cron invocation logs when it ends: which trigger fired it
/// and where its time went. Without it a firing left no line at all in
/// Workers Logs, so a cadence that was not the configured one (the trigger
/// changed to every five minutes kept firing as `*/15` for an hour while
/// Cloudflare propagated it) could only be seen by tailing for `event.cron`.
/// Modules that took under a second are left out.
fn tick_line(cron: &str, total: time::Duration, took: &[(String, time::Duration)]) -> String {
    let busy: Vec<String> = took
        .iter()
        .filter(|(_, spent)| *spent >= time::Duration::SECOND)
        .map(|(name, spent)| format!("{name}={}s", spent.whole_seconds()))
        .collect();
    format!(
        "scheduled: cron=\"{cron}\" took={}s modules=[{}]",
        total.whole_seconds(),
        busy.join(" ")
    )
}

/// What one cron invocation may spend, split across the modules in order
/// with each module's unspent share rolling forward ([`ScheduledSplit`]).
///
/// Sized for **Workers Paid** (developers.cloudflare.com/workers/platform/
/// limits/): a scheduled invocation has a 15-minute wall, and a paid
/// Worker may make 10,000 subrequests per invocation (Fetch *and* calls to
/// D1, R2 and KV). The wall here is 12 minutes ([`DEFAULT_SCHEDULED_WALL`]),
/// three kept for the compile drain's tail ([`module_budget`]) and the
/// finish. The steps are 2,000 ([`DEFAULT_SCHEDULED_SUBREQUESTS`]), a fifth
/// of the real limit, because a step is an *estimate*: a compile is counted
/// as 8 (9 with the triage classifier) though its D1 and R2 reads and
/// writes come to more, and a GitHub call is counted as one though filing
/// what it read writes the ledger too.
///
/// Both are deployment settings, `SCHEDULED_WALL_SECS` and
/// `SCHEDULED_SUBREQUESTS` (docs/deploy.md), clamped to
/// [`SCHEDULED_WALL_RANGE`] and [`SCHEDULED_SUBREQUESTS_RANGE`]; a value
/// that is blank or not a number is the default. A deployment on Workers
/// Free sets them to `300` and `45` (its limit is 50 subrequests).
///
/// The cron fires every five minutes, so invocations can overlap: a
/// compile drain runs up to [`livingbrain_compiler::WALL_AFTER_BUDGET`]
/// past its share's deadline. That is safe by leases, not by timing — a
/// compile job is claimed with a compare-and-set under a 15-minute lease
/// (the invocation can never outlive it), a GitHub queue item likewise
/// with a lease token its completion must present, and a repository's
/// backfill pass under a lease in its cursor (`livingbrain-github-app`).
fn scheduled_limits(wall_secs: Option<&str>, subrequests: Option<&str>) -> ScheduledLimits {
    let wall = parse_clamped(wall_secs, SCHEDULED_WALL_RANGE)
        .unwrap_or(DEFAULT_SCHEDULED_WALL.whole_seconds());
    let subrequests = parse_clamped(subrequests, SCHEDULED_SUBREQUESTS_RANGE)
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(DEFAULT_SCHEDULED_SUBREQUESTS);
    ScheduledLimits {
        wall: Some(time::Duration::seconds(wall)),
        subrequests: Some(subrequests),
    }
}

/// A whole number from a deployment setting, clamped into `range`; `None`
/// when it is absent, blank or not a number.
fn parse_clamped(raw: Option<&str>, range: (i64, i64)) -> Option<i64> {
    raw.map(str::trim)
        .filter(|raw| !raw.is_empty())
        .and_then(|raw| raw.parse::<i64>().ok())
        .map(|n| n.clamp(range.0, range.1))
}

/// Cloudflare's wall for one scheduled invocation, on every plan.
const CRON_HARD_WALL: time::Duration = time::Duration::minutes(15);
/// The invocation's wall unless `SCHEDULED_WALL_SECS` says otherwise.
const DEFAULT_SCHEDULED_WALL: time::Duration = time::Duration::minutes(12);
/// What `SCHEDULED_WALL_SECS` is clamped to: at least 30 s, at most 14
/// minutes, so one is always left under [`CRON_HARD_WALL`].
const SCHEDULED_WALL_RANGE: (i64, i64) = (30, 14 * 60);
/// The invocation's steps unless `SCHEDULED_SUBREQUESTS` says otherwise.
const DEFAULT_SCHEDULED_SUBREQUESTS: u32 = 2_000;
/// What `SCHEDULED_SUBREQUESTS` is clamped to: at least 10, at most 5,000 —
/// half the paid plan's 10,000, since a step undercounts (see
/// [`scheduled_limits`]).
const SCHEDULED_SUBREQUESTS_RANGE: (i64, i64) = (10, 5_000);

/// One module's budget: its share of the split, except that the compile
/// drain's deadline is held early enough for its tail to fit.
///
/// The compiler starts compiles until its share's deadline and lets them
/// run [`livingbrain_compiler::WALL_AFTER_BUDGET`] (9 minutes) past it, so
/// the deadline it is handed must leave that much (and 30 s for the finish)
/// under [`CRON_HARD_WALL`] — at most 5.5 minutes into the invocation,
/// however long the wall or the modules before it took. Every other module
/// stops at its deadline and keeps its share as it is.
fn module_budget(module: &str, share: ScheduledBudget, start: OffsetDateTime) -> ScheduledBudget {
    if module != CODING_AGENTS_MODULE {
        return share;
    }
    let latest = start + CRON_HARD_WALL
        - livingbrain_compiler::WALL_AFTER_BUDGET
        - time::Duration::seconds(30);
    let deadline = share.deadline().map(|deadline| deadline.min(latest));
    ScheduledBudget::new(deadline, share.remaining_subrequests())
}

/// The compile drain's module name (`livingbrain_api::CodingAgents`).
const CODING_AGENTS_MODULE: &str = "coding-agents";

#[cfg(test)]
mod tests {
    use super::*;

    /// The one allowlist an actions token is held to (issue #234): the four
    /// read-only endpoints a ChatGPT custom GPT's Actions call, each under
    /// both path spellings a request can arrive with — the whole one and the
    /// prefix-stripped view the nested surface router sees.
    #[test]
    fn an_actions_token_may_call_exactly_four_read_only_routes() {
        for (allowed, method, path) in [
            (true, HttpMethod::GET, "/v1/search"),
            (true, HttpMethod::GET, "/v1/search?q=launch&limit=10"),
            (true, HttpMethod::HEAD, "/v1/search"),
            (true, HttpMethod::GET, "/v1/facts"),
            (true, HttpMethod::GET, "/v1/facts?limit=50"),
            (true, HttpMethod::POST, "/v1/ask"),
            (true, HttpMethod::GET, "/v1/pages/launch-plan"),
            (true, HttpMethod::GET, "/launch-plan"),
            // The same four, refused under every other method.
            (false, HttpMethod::POST, "/v1/search"),
            (false, HttpMethod::PUT, "/v1/ask"),
            (false, HttpMethod::DELETE, "/v1/facts"),
            (false, HttpMethod::POST, "/v1/pages/launch-plan"),
        ] {
            assert_eq!(
                actions_request_allowed(&method, path),
                allowed,
                "{method} {path}"
            );
        }
        // Everything else the four modules serve: the page list, a write on
        // a page, the MCP endpoint nested under the same mount, a nested
        // path, and the bare mount itself.
        for (method, path) in [
            (HttpMethod::GET, "/v1/pages"),
            (HttpMethod::GET, "/v1/pages/"),
            (HttpMethod::GET, "/"),
            (HttpMethod::PUT, "/v1/pages/launch-plan"),
            (HttpMethod::PUT, "/launch-plan"),
            (HttpMethod::GET, "/v1/pages/mcp"),
            (HttpMethod::POST, "/v1/pages/mcp"),
            (HttpMethod::GET, "/v1/pages/mcp/tools"),
            (HttpMethod::GET, "/v1/pages/a/nested/path"),
            (HttpMethod::GET, "/v1/notes"),
        ] {
            assert!(
                !actions_request_allowed(&method, path),
                "{method} {path} reached"
            );
        }
    }

    #[test]
    fn a_tick_logs_its_trigger_and_where_its_time_went() {
        let took = vec![
            ("waitlist".to_owned(), time::Duration::milliseconds(20)),
            ("coding-agents".to_owned(), time::Duration::seconds(212)),
            ("github".to_owned(), time::Duration::seconds(31)),
        ];
        assert_eq!(
            tick_line("*/5 * * * *", time::Duration::seconds(250), &took),
            "scheduled: cron=\"*/5 * * * *\" took=250s modules=[coding-agents=212s github=31s]"
        );
    }

    /// The cadence the scheduled limits are sized for: a five-minute cron
    /// beside the daily one, declared once at the top level and inherited
    /// by staging and production (a `[env.*.triggers]` would replace it).
    #[test]
    fn the_cron_fires_every_five_minutes_in_every_environment() {
        let config = include_str!("../wrangler.toml");
        let crons: Vec<&str> = config
            .lines()
            .filter(|line| line.trim_start().starts_with("crons"))
            .collect();
        assert_eq!(crons, vec![r#"crons = ["19 4 * * *", "*/5 * * * *"]"#]);
        assert!(
            !config.lines().any(|line| {
                let line = line.trim();
                line.starts_with("[env.") && line.ends_with(".triggers]")
            }),
            "an environment must not override the cron"
        );
        // The compile drain's start window (5.5 minutes) covers the gap
        // between two firings, so the pool never sits idle between them.
        assert!(
            CRON_HARD_WALL - livingbrain_compiler::WALL_AFTER_BUDGET - time::Duration::seconds(30)
                >= time::Duration::minutes(5)
        );
    }
    use cratefield_kms::{Dek, LocalFileKms};

    #[test]
    fn the_scheduled_limits_default_to_the_paid_plan() {
        let limits = scheduled_limits(None, None);
        assert_eq!(limits.wall, Some(time::Duration::minutes(12)));
        assert_eq!(limits.subrequests, Some(2_000));
        // Blank or not a number is the default, not zero.
        let limits = scheduled_limits(Some("  "), Some("lots"));
        assert_eq!(limits.wall, Some(time::Duration::minutes(12)));
        assert_eq!(limits.subrequests, Some(2_000));
        assert!(
            DEFAULT_SCHEDULED_WALL < CRON_HARD_WALL,
            "headroom under Cloudflare's wall"
        );
    }

    #[test]
    fn the_scheduled_limits_take_the_env_and_clamp_it() {
        // A free-plan deployment.
        let free = scheduled_limits(Some("300"), Some(" 45 "));
        assert_eq!(free.wall, Some(time::Duration::minutes(5)));
        assert_eq!(free.subrequests, Some(45));
        // Past either end: clamped, never past Cloudflare's wall or half
        // the paid plan's subrequests.
        let high = scheduled_limits(Some("3600"), Some("1000000"));
        assert_eq!(high.wall, Some(time::Duration::minutes(14)));
        assert_eq!(high.subrequests, Some(5_000));
        let low = scheduled_limits(Some("0"), Some("-5"));
        assert_eq!(low.wall, Some(time::Duration::seconds(30)));
        assert_eq!(low.subrequests, Some(10));
    }

    #[test]
    fn the_split_hands_the_compiler_a_paid_share() {
        // Mounted order: the compiler is third from last (then `model-jobs`
        // and `github`), so with the modules before it spending nothing it
        // gets a third of what is left.
        let start = OffsetDateTime::UNIX_EPOCH;
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
        let names: Vec<&str> = harness.modules().iter().map(|m| m.name()).collect();
        let at = names
            .iter()
            .position(|name| *name == CODING_AGENTS_MODULE)
            .expect("the compiler is mounted");
        assert_eq!(names.last(), Some(&"github"), "github inherits the rest");
        let mut split = ScheduledSplit::new(scheduled_limits(None, None), start, names.len());
        for _ in 0..at {
            let share = split.next(start);
            split.settle(&share);
        }
        let compiler = module_budget(CODING_AGENTS_MODULE, split.next(start), start);
        let left = names.len() - at;
        assert_eq!(
            compiler.remaining_subrequests(),
            Some(2_000_u32.div_ceil(u32::try_from(left).unwrap()))
        );
        // 8 steps a compile: far more than six connections and the clock
        // let one invocation start, so time is the bound, not steps.
        assert!(compiler.remaining_subrequests().unwrap() / 9 >= 20 * 3);
        let deadline = compiler.deadline().expect("bounded");
        assert!(deadline <= start + time::Duration::seconds(330));
        assert!(
            deadline + livingbrain_compiler::WALL_AFTER_BUDGET < start + CRON_HARD_WALL,
            "the drain's tail fits in the invocation"
        );
        // The last module gets everything left.
        split.settle(&compiler);
        for _ in at + 1..names.len() - 1 {
            let share = split.next(start);
            split.settle(&share);
        }
        let github = module_budget("github", split.next(start), start);
        assert_eq!(github.remaining_subrequests(), Some(2_000));
        assert_eq!(github.deadline(), Some(start + time::Duration::minutes(12)));
    }

    #[test]
    fn the_compile_deadline_is_held_under_the_wall_whatever_ran_before() {
        let start = OffsetDateTime::UNIX_EPOCH;
        // The modules before it took eight minutes: its even share would
        // end at 9:20, and nine more minutes would pass Cloudflare's wall.
        let late = ScheduledBudget::new(Some(start + time::Duration::seconds(560)), Some(600));
        let held = module_budget(CODING_AGENTS_MODULE, late, start);
        assert_eq!(held.deadline(), Some(start + time::Duration::seconds(330)));
        assert_eq!(held.remaining_subrequests(), Some(600));
        // An earlier deadline is kept as it is; other modules are untouched.
        let early = ScheduledBudget::new(Some(start + time::Duration::seconds(200)), Some(7));
        let kept = module_budget(CODING_AGENTS_MODULE, early, start);
        assert_eq!(kept.deadline(), Some(start + time::Duration::seconds(200)));
        let other = ScheduledBudget::new(Some(start + time::Duration::seconds(560)), Some(7));
        assert_eq!(
            module_budget("github", other, start).deadline(),
            Some(start + time::Duration::seconds(560))
        );
    }

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
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
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
        assert!(names.contains(&"coding-agents"), "{names:?}");
        assert!(serves_cli(&harness, "/v1/coding-agents/queue"));
        assert!(serves_cli(&harness, "/v1/coding-agents/process"));
        assert!(!serves_cli(&harness, "/v1/coding-agentsx/queue"));
        assert!(names.contains(&"model-jobs"), "{names:?}");
        assert!(serves_cli(&harness, "/v1/model-jobs"));
        assert!(serves_cli(&harness, "/v1/model-jobs/next"));
        assert!(serves_cli(&harness, "/v1/model-jobs/mj-1/result"));
        // Issue #72: the grant a CLI logs in through, and its credential.
        assert!(names.contains(&"device-auth"), "{names:?}");
        assert!(names.contains(&"tokens"), "{names:?}");
        // The budgets and limits, beside the ledger they are read against.
        assert!(names.contains(&"settings"), "{names:?}");
        assert!(names.contains(&"usage"), "{names:?}");
        assert!(names.contains(&"github"), "{names:?}");
        // Connected apps (issues #51 and #146): the connection, the
        // publication and the five tools.
        for name in ["apps", "public", "app-tools"] {
            assert!(names.contains(&name), "{name} is mounted: {names:?}");
        }
        assert!(serves_cli(&harness, "/v1/apps"));
        assert!(serves_cli(&harness, "/v1/apps/earthos/subjects/user:1"));
        assert!(serves_cli(&harness, "/v1/public/pages"));
        assert!(serves_cli(&harness, "/v1/tools/brain_answer_public"));
        assert!(!serves_cli(&harness, "/v1/toolsx/brain_answer_public"));
    }

    /// The tools are served at `/v1/tools/…` (the contract SupportGenius and
    /// EarthOS speak) and mounted as `app-tools`; only that prefix moves.
    #[test]
    fn the_tools_path_is_readdressed_to_the_app_tools_mount() {
        assert_eq!(
            rewrite_tools_path("/v1/tools/brain_answer_public"),
            "/v1/app-tools/brain_answer_public"
        );
        assert_eq!(rewrite_tools_path("/v1/tools"), "/v1/app-tools");
        for untouched in [
            "/v1/toolsx/brain_answer_public",
            "/v1/app-tools/brain_answer_public",
            "/v1/notes",
            "/v1/apps",
        ] {
            assert_eq!(rewrite_tools_path(untouched), untouched);
        }
    }

    /// The five tools answer end to end through the venture's own
    /// composition, at the path the clients call: the Worker's bundle, minus
    /// the runtime. A credential for one app, a subject of its own.
    #[test]
    fn the_worker_composition_serves_the_tools_module_and_its_migration() {
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
        let module = harness
            .modules()
            .iter()
            .find(|module| module.name() == "app-tools")
            .expect("app-tools is mounted");
        assert!(module.requires().contains(&cratefield_core::Port::Blob));
        assert!(
            harness
                .modules()
                .iter()
                .any(|module| module.name() == "pages"
                    && module.tables().contains(&"app_memories")
                    && module.tables().contains(&"public_pages"))
        );
    }

    /// A migration's SQL without its `--` comments, its words joined by
    /// single spaces: what a database runs.
    fn statements(sql: &str) -> String {
        sql.lines()
            .map(|line| line.split_once("--").map_or(line, |(code, _)| code))
            .flat_map(str::split_whitespace)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every migration a mounted module ships is collected into
    /// `migrations/` — the directory `wrangler d1 migrations apply` reads —
    /// with the same statements (comments and spacing aside: a collected
    /// file is frozen once applied, and a reworded comment in the module's
    /// copy changes no schema), and pinned in `.harness-lock.json`. A module that
    /// gains a migration nobody collects passes every module test and then
    /// fails in production on a missing table (#167, fixed by #170); this
    /// is the check that would have caught it.
    #[test]
    fn every_module_migration_is_collected_for_d1() {
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let lock: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".harness-lock.json")).expect("the lock file"),
        )
        .expect("the lock is JSON");
        let mut missing = Vec::new();
        for module in harness.modules() {
            for migration in module.migrations().sqlite {
                let key = format!("{}/{}", module.name(), migration.id);
                let Some(file) = lock
                    .get(&key)
                    .and_then(|entry| entry.get("file"))
                    .and_then(serde_json::Value::as_str)
                else {
                    missing.push(format!(
                        "{key} ({}) is not in .harness-lock.json",
                        migration.name
                    ));
                    continue;
                };
                let expected =
                    format!("_{}_{}_{}.sql", module.name(), migration.id, migration.name);
                assert!(file.ends_with(&expected), "{key} is collected as {file}");
                match std::fs::read_to_string(dir.join(file)) {
                    Ok(collected) if statements(&collected) == statements(migration.sql) => {}
                    Ok(_) => missing.push(format!("{file} differs from {key}'s SQL")),
                    Err(_) => missing.push(format!("{file} ({key}) is missing")),
                }
            }
        }
        assert!(
            missing.is_empty(),
            "collect these into crates/livingbrain-venture/migrations (next number, \
             `<n>_<module>_<id>_<name>.sql`, plus a .harness-lock.json entry):\n{}",
            missing.join("\n")
        );
    }

    /// The lock as production last applied it (40b538f): every entry here
    /// is in D1 under its file name, so its bytes are frozen. A later lock
    /// may add entries; it may not repoint or re-hash one of these.
    const APPLIED_LOCK: &[(&str, &str, &str)] = &[
        (
            "workspaces/0001",
            "0001_workspaces_0001_init.sql",
            "219fb61be95955e9eb48d18f4c44c7365128d91c517d85caa6c3e45da1baa24e",
        ),
        (
            "pages/0001",
            "0002_pages_0001_init.sql",
            "cd0d65c9d2af2d6a0a6873a6cd04363ca910574d6ecf80c83cf2dd2257b96c34",
        ),
        (
            "waitlist/0001",
            "0003_waitlist_0001_init.sql",
            "504c3e013f921975e286b27f6253ac5f2b9f884bf2b89f6d077c4931760c54ea",
        ),
        (
            "waitlist/0002",
            "0004_waitlist_0002_entry_generation.sql",
            "33dc5cf8d71a3c484e3fc8f838f1a8b3a04f2cb8281b5b1e149ad9e4e7fb41d4",
        ),
        (
            "waitlist/0003",
            "0005_waitlist_0003_mail_cooldown.sql",
            "8f8319f4f0613d9f08ec9f17e5a2356f33efedb4a49e3e5d4fb1805f878d3507",
        ),
        (
            "waitlist/0004",
            "0006_waitlist_0004_position_lock.sql",
            "44eeab454a0a322e77a6cc81640ab25fb4fb038af9eea8458224925579c83414",
        ),
        (
            "waitlist/0005",
            "0007_waitlist_0005_anonymisable_entry.sql",
            "2b129c62bcacc0c0db77e9af1f07d1034057d8a3ce1d4eee51ada0dfafec3c63",
        ),
        (
            "waitlist/0006",
            "0008_waitlist_0006_position_counter.sql",
            "671812656c87303ee53fd845c3745bbe9d47d12ffecc9e7485d3395541cb821a",
        ),
        (
            "pages/0002",
            "0009_pages_0002_scope_keys.sql",
            "8af0624508cb33d326fc87b37d6a11f4f06c12f6c04843b11a46c3f679219b37",
        ),
        (
            "workspaces/0002",
            "0010_workspaces_0002_identities.sql",
            "de7f6811c3753bd643158130f3d46aa16cfe7159e8f0f942c3b669ea57519168",
        ),
        (
            "models/0001",
            "0011_models_0001_init.sql",
            "f32f60c7db7c85c6cbf46f1fd69d6ba1ca9d57f832fc7d1bd8053de6282f23be",
        ),
        (
            "workspaces/0003",
            "0012_workspaces_0003_slack_app.sql",
            "b6ad4a673b3ca4c04ee8853add253ee19036aa6ad33b61c5139cc9cc262825fb",
        ),
        (
            "pages/0003",
            "0013_pages_0003_sources.sql",
            "88c9b9a47025b9403f13cae3cc7209913392a9653630d8d0e0ccbb2477119d8f",
        ),
        (
            "device-auth/0001",
            "0014_device-auth_0001_init.sql",
            "e91aedf2fb661c50e9db080ad48e6daf5736fd02f3bfe32ae7a34d5ab1395c94",
        ),
        (
            "tokens/0001",
            "0015_tokens_0001_init.sql",
            "0ea8243b2b7ab242454c815e5a6a4d9c114db8c997fbca96613b7115feef26e0",
        ),
        (
            "models/0002",
            "0016_models_0002_provider_wire.sql",
            "0027f24324aa342d22e31df8ab965de17f566fb0665f91592a4fdd3e9ef43563",
        ),
        (
            "pages/0004",
            "0017_pages_0004_source_ledger.sql",
            "75795d68d54db610bfbbad26ee4da5ed28b78553f7b9cca12be2e78f8eafba23",
        ),
        (
            "tools/0001",
            "0018_tools_0001_init.sql",
            "0a4d2c03a8af4e1e8ae3d168fae7947858e893a9c59bb5d9334a58abb86b0307",
        ),
        (
            "usage/0001",
            "0019_usage_0001_init.sql",
            "cdb8adf5ba7ab4697f77d5b20b7e2e315589d165c0bcadee0771a233f814d8cd",
        ),
        (
            "pages/0005",
            "0020_pages_0005_source_project.sql",
            "9c186496a70a760cf6c22ecaafd028fdaddf61188b2c3ba734c6d1d2f4732e61",
        ),
        (
            "coding-agents/0001",
            "0021_coding-agents_0001_init.sql",
            "950a2cbca645ab8c41d9559e8fd4a53548627291b50135952c9697ed884199b2",
        ),
        (
            "pages/0006",
            "0022_pages_0006_facts.sql",
            "7eb6480af9e61e81e876a16e85e67cfd1742f6b2a87e74780ccab75905725533",
        ),
        (
            "workspaces/0005",
            "0023_workspaces_0005_members.sql",
            "ae6fc1ffa6eed6f2db27d527be294dfa3bd77cff6ed842036c0cf4a18cca1da4",
        ),
        (
            "models/0003",
            "0024_models_0003_local_runner.sql",
            "3b69b0af7ac7149c00ac5a7b6ea1390b445589d7e560a735f077dc9302ab95bf",
        ),
        (
            "coding-agents/0002",
            "0025_coding-agents_0002_waiting_runner.sql",
            "acc417c7c2ef7d655c54ccc7cd87b5a639729f36a3e1d7eba511170d71663d46",
        ),
        (
            "usage/0002",
            "0026_usage_0002_budgets.sql",
            "4ee61f19b0304b8c4708c43e5ef54e8523868039927af5a9bba3069af09b4eb6",
        ),
        (
            "workspaces/0004",
            "0027_workspaces_0004_turn.sql",
            "59203b60ca937e502972dab8dbf4d7f04526c886147cd4927a5ba15883cf2151",
        ),
        (
            "github/0001",
            "0028_github_0001_init.sql",
            "ca01e020082913bc302ef21d87940c15904755376bd16bdcf9633a2243d96706",
        ),
        (
            "coding-agents/0003",
            "0029_coding-agents_0003_triage.sql",
            "017f7272d645464eeb587507942d1ef38263586a8788e327012ebec7d69237e2",
        ),
        (
            "usage/0003",
            "0030_usage_0003_triage.sql",
            "17a0aad6f1281cc154c30dccdb5113562abe021adf8ab45a2558426a50c3dce8",
        ),
        (
            "pages/0007",
            "0031_pages_0007_page_titles.sql",
            "0818fa65d741d39c58b6cd2bde120c78d27834f727c2282ba0fd1a197ab8c52d",
        ),
        (
            "github/0002",
            "0032_github_0002_shared_app.sql",
            "5bd3235d2478f5ed3117bb003cc8d8c3341cc708137b236e07ec90f4dbb81551",
        ),
        (
            "workspaces/0006",
            "0033_workspaces_0006_profiles.sql",
            "d75f53116b83d03df857501b773f7a9fbf6f9eeef25b2802f3d62e0b2d569162",
        ),
        (
            "coding-agents/0004",
            "0034_coding-agents_0004_parallel.sql",
            "0ed68a4aae1c0c01591daff2062df49c21c139de94c9883b0775b7277c16a8e1",
        ),
        (
            "usage/0004",
            "0035_usage_0004_concurrency.sql",
            "cbc6032312f803183c645f459ecf510d13a2c0dcae88bd5a76f61c2ab35befa3",
        ),
        (
            "models/0004",
            "0036_models_0004_jev.sql",
            "dd9ff4975145a6f0c7140b57def634e33bba7e5fb41b1980e1ed78d0ae8e06bf",
        ),
        (
            "waitlist/0007",
            "0037_waitlist_0007_lock_backfill.sql",
            "6785fee9ba5dee4ab7eba92f36b42748facae3c92837df3f4b67ff5280a80006",
        ),
        (
            "access/0001",
            "0038_access_0001_service-accounts.sql",
            "1b6a02f028fe3571806e3e2a627d61fe786344eab044aa431b8f27014a15c7be",
        ),
        (
            "access/0002",
            "0039_access_0002_grants.sql",
            "f36a4321d0688232a00e8cd7b566b652a522bb15ad44147750bd27133d9aa8c4",
        ),
        (
            "access/0003",
            "0040_access_0003_audit.sql",
            "30a09580794d2f4445f360d5db1db90edd709ac65e6a29898b0c8ec7011d5e90",
        ),
        (
            "coding-agents/0005",
            "0041_coding-agents_0005_requeue_bad_output.sql",
            "fe2c0ed5118d0fc9aebd3cc18c4dc66120e462ac0f1f87451c5462a5692fa192",
        ),
        (
            "pages/0008",
            "0042_pages_0008_dreaming.sql",
            "3f3048117a3c55f6ef0782ef5341c544094683c34ca13f6bfde473df1bdffc3c",
        ),
        (
            "pages/0009",
            "0043_pages_0009_dream_cursor.sql",
            "382c8cc35ffe998fc93f7fd783c4d44b62141590a048de8911836d2c216fffe7",
        ),
        (
            "pages/0010",
            "0044_pages_0010_rule_scan.sql",
            "209823941101ddd017f2f09acc6d7e7c8735a7dc2dba87537aa91d1c7899a678",
        ),
        (
            "workspaces/0007",
            "0045_workspaces_0007_delete_workspace.sql",
            "3bb7dd52f91946bb6b07a4a892d81b0ec11e21b62a9f42ef8c3a04763b6443f7",
        ),
        (
            "discord/0001",
            "0046_discord_0001_init.sql",
            "49474a0585db89c5801f1281fdb67868b596043a22f54439fde34b59e821a162",
        ),
        (
            "coding-agents/0006",
            "0047_coding-agents_0006_dreaming_settings.sql",
            "cf5077af3b7ce774bb533bb766a4700273003cc91a63dcabcbf1212d8fce174e",
        ),
        (
            "tokens/0002",
            "0048_tokens_0002_expiry_audience.sql",
            "f30abbc7cd219893851e75a86db43dfdf2336b1f892312b01f3f616ce596faa1",
        ),
        (
            "oauth/0001",
            "0049_oauth_0001_init.sql",
            "006c64b0d37111553f2d91a7dc0635b620d7b5a7a85c05c6e5dbc58f9f432621",
        ),
        (
            "coding-agents/0007",
            "0050_coding-agents_0007_source_compile_jobs.sql",
            "7cb342603c3517a407593e04902810dfaf0eff36271a37fc115e0702286d7858",
        ),
        (
            "pages/0011",
            "0051_pages_0011_activity.sql",
            "961da0889149174c1a12b3a79d51eaf48b6e42f5a9247d45300d489538cdaa5b",
        ),
        (
            "oauth/0002",
            "0052_oauth_0002_metadata_expiry.sql",
            "9d8925bdf22ba1867116e233cd06958565a7e92c21540a44f54e9e36c5492181",
        ),
        (
            "pages/0012",
            "0053_pages_0012_apps.sql",
            "8ca3f25e78a59c108f67326b9ab1e85d6b70701cf26dc3ec6c5be2a29e626325",
        ),
        (
            "coding-agents/0008",
            "0054_coding-agents_0008_subject_aliases.sql",
            "fe6b05c572d489b2239955bd675eec5847d0d46658e1055b77dfcd833d8dc48c",
        ),
    ];

    /// Applied migrations never change. The guard checks two things: every
    /// lock pin matches its file's bytes, and every migration that production
    /// has applied still has the file and pin it shipped with. Editing an
    /// applied migration (adding a column to `0054`, say) fails here, because
    /// D1 skips a file it has already run and would never apply the edit: the
    /// change belongs in a new, higher-numbered migration.
    #[test]
    fn applied_migrations_are_frozen_and_their_pins_match_their_files() {
        use sha2::{Digest, Sha256};
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        let lock: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(".harness-lock.json")).expect("the lock file"),
        )
        .expect("the lock is JSON");
        let lock = lock.as_object().expect("the lock is an object");
        let hash = |file: &str| -> Option<String> {
            std::fs::read(dir.join(file))
                .ok()
                .map(|bytes| format!("{:x}", Sha256::digest(&bytes)))
        };
        let mut problems = Vec::new();
        for (key, entry) in lock {
            let file = entry
                .get("file")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let pin = entry
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            match hash(file) {
                Some(actual) if actual == pin => {}
                Some(actual) => problems.push(format!(
                    "{key}: {file} hashes to {actual}, but the lock pins {pin}"
                )),
                None => problems.push(format!("{key}: {file} is missing")),
            }
        }
        for (key, file, pin) in APPLIED_LOCK {
            match lock
                .get(*key)
                .and_then(|entry| entry.get("file"))
                .and_then(serde_json::Value::as_str)
            {
                Some(now) if now == *file => {}
                Some(now) => problems.push(format!(
                    "{key} was applied as {file} and the lock now names {now}"
                )),
                None => problems.push(format!(
                    "{key} was applied as {file} and is gone from the lock"
                )),
            }
            let pinned = lock
                .get(*key)
                .and_then(|entry| entry.get("sha256"))
                .and_then(serde_json::Value::as_str);
            if pinned != Some(*pin) {
                problems.push(format!(
                    "{key} was applied with sha256 {pin} and the lock now pins {}",
                    pinned.unwrap_or("nothing")
                ));
            }
            if hash(file).as_deref() != Some(*pin) {
                problems.push(format!("{file} no longer has the bytes production applied"));
            }
        }
        assert!(
            problems.is_empty(),
            "applied migrations changed; add a new migration instead:\n{}",
            problems.join("\n")
        );
    }

    /// The avatar uploads and the read route take the rebinding stack, with
    /// or without a key ring; nothing near them does.
    #[test]
    fn the_avatar_routes_divert_and_nothing_near_them_does() {
        for path in [
            "/v1/me/avatar",
            "/v1/workspaces/current/avatar",
            "/v1/avatars/av_01HZZZBBBBBBBBBBBBBBBBBBBB",
        ] {
            assert!(serves_avatars(path), "{path}");
        }
        for path in [
            "/v1/me",
            "/v1/workspaces/current",
            "/v1/workspaces/members",
            "/v1/avatars/",
            "/v1/avatars",
            "/v1/avatars/av_x/extra",
            "/v1/me/avatarx",
        ] {
            assert!(!serves_avatars(path), "{path}");
        }
        let (harness, _, _) = build(unconfigured_mail(), None, None, None);
        let names: Vec<&str> = harness.modules().iter().map(|m| m.name()).collect();
        assert!(names.contains(&"avatars"), "{names:?}");

        // Only a read of an avatar is cached, and only privately.
        let image = "/v1/avatars/av_01HZZZBBBBBBBBBBBBBBBBBBBB";
        let policy = avatar_cache_control("GET", image).expect("an avatar read is cached");
        assert!(policy.starts_with("private"), "{policy}");
        assert!(policy.contains("immutable"), "{policy}");
        assert!(avatar_cache_control("HEAD", image).is_some());
        for (method, path) in [
            ("PUT", "/v1/me/avatar"),
            ("GET", "/v1/me/avatar"),
            ("DELETE", image),
            ("GET", "/v1/me"),
            ("GET", "/v1/avatars/"),
        ] {
            assert_eq!(avatar_cache_control(method, path), None, "{method} {path}");
        }
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
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
        assert!(
            serves_cli(&harness, "/v1/sources/01HZZZBBBBBBBBBBBBBBBBBBB"),
            "a citation must divert to serve_cli"
        );
        assert!(
            serves_cli(&harness, "/v1/sources"),
            "the bare path still diverts: it is the listing (`GET /v1/sources`)"
        );
        assert!(
            serves_cli(&harness, "/v1/sources/projects"),
            "the projects facet diverts with the citation route"
        );
        assert!(
            serves_cli(&harness, "/v1/facts/fact-01HZZZBBBBBBBBBBBBBBBBBBB/retract"),
            "a retraction diverts with the facts route"
        );
        assert!(
            !serves_cli(&harness, "/v1/factsother"),
            "a sibling path that only shares the prefix does not divert"
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

    /// The brain map's route (`GET /v1/graph`) diverts to the CLI stack like
    /// the listings it sits beside, and its module is mounted with the key
    /// ring. Without this the map would be served by `serve`, which answers
    /// 404 to an anonymous caller instead of the 401 every other route gives.
    #[test]
    fn the_graph_route_diverts_and_its_module_is_mounted() {
        let kek = LocalFileKms::from_key(Dek::generate().unwrap(), "test", "test")
            .expect("a well-formed key");
        let (harness, _, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)), None);
        assert!(serves_cli(&harness, "/v1/graph"), "the map must divert");
        assert!(
            harness.modules().iter().any(|m| m.name() == "graph"),
            "the graph module mounts with the key ring"
        );
        assert!(
            !serves_cli(&harness, "/v1/graphs"),
            "a sibling path that only shares the prefix does not divert"
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
    /// and the CLI modules are not mounted, and every other route keeps
    /// working — the CLI paths fall through to `serve`, which answers 404.
    #[test]
    fn a_missing_key_ring_leaves_the_rest_of_the_composition_serving() {
        let (harness, _, _) = build(unconfigured_mail(), None, None, None);
        let names: Vec<&str> = harness.modules().iter().map(|m| m.name()).collect();
        assert!(names.contains(&"pages"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
        for module in [
            "notes", "search", "ask", "export", "sources", "facts", "graph",
        ] {
            assert!(!names.contains(&module), "{names:?}");
        }
        for route in CLI_ROUTES {
            assert!(!serves_cli(&harness, route), "{route}");
        }
    }

    /// The device grant and the tokens module that answers its hooks, over
    /// the kit's fakes and this venture's `venture()` — the two modules the
    /// approval page and its decision forms run through.
    fn device_kit() -> cratefield_testing::TestHarness {
        let ports = DevicePorts::new();
        cratefield_testing::TestHarness::with_builder(
            vec![
                Box::new(device_auth_module(&ports)),
                Box::new(tokens_module(ports)),
            ],
            |builder| builder.venture(venture()),
            |_| {},
        )
    }

    /// One request through the kit's router, with the headers a browser
    /// on `https://app.livingbrain.wiki` would send.
    async fn send(
        kit: &cratefield_testing::TestHarness,
        method: HttpMethod,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> AxumResponse {
        let mut builder = HttpRequest::builder()
            .method(method)
            .uri(format!("https://app.livingbrain.wiki{path}"))
            .header(header::HOST, "app.livingbrain.wiki");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        let request = builder
            .body(Body::from(body.to_owned()))
            .expect("a well-formed request");
        kit.router
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible")
    }

    /// The production failure, end to end through the harness and the
    /// pinned device-auth module: the page leaves the harness carrying
    /// `no-referrer`, a form posted from a `no-referrer` page arrives with
    /// `Origin: null`, and the module refuses that with the 403 people saw.
    /// The override replaces the page's policy with `same-origin`, under
    /// which the same form arrives with this host's real origin — and the
    /// module lets that through to the approver (here, an anonymous visitor
    /// is sent to sign in: past the origin check, not stopped by it).
    #[pollster::test]
    async fn the_approval_form_posts_a_real_origin_and_passes_the_check() {
        let kit = device_kit();

        // The page: the harness stamps `no-referrer`, the venture swaps it.
        let page = send(
            &kit,
            HttpMethod::GET,
            "/v1/device-auth?user_code=ABCD-EFGH",
            &[("sec-fetch-site", "none")],
            "",
        )
        .await;
        let mut headers = page.headers().clone();
        assert_eq!(
            headers.get("referrer-policy").map(HeaderValue::as_bytes),
            Some(&b"no-referrer"[..]),
            "the harness's stamp, which the fix overrides"
        );
        let policy = page_referrer_policy("GET", "/v1/device-auth")
            .expect("the approval page gets its own policy");
        headers.insert("referrer-policy", HeaderValue::from_static(policy));
        // Under `same-origin` the Fetch standard serializes a same-origin
        // POST's `Origin` as the page's own origin; under `no-referrer` it
        // is `null`. Nothing laxer than that is allowed through.
        assert_eq!(
            headers.get("referrer-policy").map(HeaderValue::as_bytes),
            Some(&b"same-origin"[..])
        );

        let form = [
            ("sec-fetch-site", "same-origin"),
            ("content-type", "application/x-www-form-urlencoded"),
        ];
        // What the browser sent while the page carried `no-referrer`.
        let mut null_origin = form.to_vec();
        null_origin.push(("origin", "null"));
        let refused = send(
            &kit,
            HttpMethod::POST,
            "/v1/device-auth/approve",
            &null_origin,
            "user_code=ABCD-EFGH",
        )
        .await;
        assert_eq!(refused.status(), 403, "Origin: null is refused");
        let body = to_bytes(refused.into_body(), 64 * 1024)
            .await
            .expect("a problem body");
        let body = String::from_utf8_lossy(&body);
        assert!(
            body.contains("cross-site-request") && body.contains("origin is null"),
            "the production 403: {body}"
        );

        // What it sends once the page carries `same-origin`.
        let mut real_origin = form.to_vec();
        real_origin.push(("origin", "https://app.livingbrain.wiki"));
        let accepted = send(
            &kit,
            HttpMethod::POST,
            "/v1/device-auth/approve",
            &real_origin,
            "user_code=ABCD-EFGH",
        )
        .await;
        assert_eq!(
            accepted.status(),
            303,
            "past the origin check: an anonymous approver is sent to sign in"
        );
    }

    /// The override is the page's alone. The decision posts and every other
    /// `/v1` route — the email-verify page, which carries a live sign-in
    /// token in its URL, above all — keep the harness's `no-referrer`.
    #[test]
    fn only_the_device_page_relaxes_its_referrer_policy() {
        assert_eq!(
            page_referrer_policy("GET", "/v1/device-auth"),
            Some("same-origin")
        );
        assert_eq!(
            page_referrer_policy("HEAD", "/v1/device-auth/"),
            Some("same-origin")
        );
        for (method, path) in [
            ("POST", "/v1/device-auth"),
            ("POST", "/v1/device-auth/approve"),
            ("POST", "/v1/device-auth/deny"),
            ("POST", "/v1/device-auth/code"),
            ("POST", "/v1/device-auth/token"),
            ("GET", "/v1/device-auth/approve"),
            ("GET", "/v1/device-authx"),
            ("GET", "/v1/workspaces/email/verify"),
            ("POST", "/v1/workspaces/email/verify"),
            ("GET", "/v1/tokens"),
        ] {
            assert_eq!(page_referrer_policy(method, path), None, "{method} {path}");
        }
    }

    /// An approver that has already signed the visitor in, so the page
    /// renders the module's own documents instead of the `303` to sign-in.
    /// The session check itself is the tokens module's and is tested there.
    struct SignedIn;

    #[async_trait::async_trait]
    impl cratefield_module_device_auth::Approver for SignedIn {
        async fn approve(
            &self,
            _headers: &HeaderMap,
            _return_to: &str,
        ) -> Result<
            cratefield_module_device_auth::Approval,
            cratefield_module_device_auth::ApproverError,
        > {
            Ok(cratefield_module_device_auth::Approval::Subject(
                "T0SPACE\u{1f}U0OWNER".to_owned(),
            ))
        }
    }

    /// [`device_kit`] with a signed-in approver: the grant this venture
    /// mounts, its one client and the tokens module's issuer.
    fn signed_in_device_kit() -> cratefield_testing::TestHarness {
        let ports = DevicePorts::new();
        let grant = DeviceAuth::builder()
            .client(DeviceClient::new(DEVICE_CLIENT))
            .random(WorkersRandom)
            .approver(SignedIn)
            .issuer(ports.issuer())
            .build();
        cratefield_testing::TestHarness::with_builder(
            vec![Box::new(grant), Box::new(tokens_module(ports))],
            |builder| builder.venture(venture()),
            |_| {},
        )
    }

    /// The response's `content-type` and body, as `fetch` would read them.
    async fn read(response: AxumResponse) -> (Option<String>, String) {
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = to_bytes(response.into_body(), 256 * 1024)
            .await
            .expect("a body");
        (content_type, String::from_utf8_lossy(&body).into_owned())
    }

    /// A fresh user code from the machine route, the way the CLI asks.
    async fn new_user_code(kit: &cratefield_testing::TestHarness) -> String {
        let response = send(
            kit,
            HttpMethod::POST,
            "/v1/device-auth/code",
            &[("content-type", "application/x-www-form-urlencoded")],
            "client_id=livingbrain-cli&name=nick%27s%20%3Cmac%3E",
        )
        .await;
        assert_eq!(response.status(), 200);
        let (_, body) = read(response).await;
        let json: serde_json::Value = serde_json::from_str(&body).expect("a JSON body");
        json["user_code"].as_str().expect("a user code").to_owned()
    }

    /// Branded the way `fetch` brands it: the same route gate, the same
    /// type gate, over what the pinned module really rendered.
    async fn brand_as_fetch(method: &str, path: &str, response: AxumResponse) -> String {
        assert!(device_pages::is_document(method, path), "{method} {path}");
        let (content_type, body) = read(response).await;
        assert!(
            device_pages::is_html(content_type.as_deref()),
            "{content_type:?}"
        );
        device_pages::brand(&body)
    }

    /// The whole browser half of `livingbrain login`, through the harness
    /// and the pinned module: what it renders is recognised and re-rendered,
    /// the forms post exactly where the module's did with the same hidden
    /// code, a decision posted from the branded form is accepted, and the
    /// device label the client chose is escaped on the way through.
    #[pollster::test]
    async fn the_device_pages_are_branded_end_to_end() {
        let kit = signed_in_device_kit();
        let code = new_user_code(&kit).await;
        let page_path = format!("/v1/device-auth?user_code={code}");
        let page = send(
            &kit,
            HttpMethod::GET,
            &page_path,
            &[("sec-fetch-site", "none")],
            "",
        )
        .await;
        assert_eq!(page.status(), 200);
        for (name, value) in [
            ("cache-control", "no-store"),
            ("x-frame-options", "DENY"),
            ("content-security-policy", "frame-ancestors 'none'"),
        ] {
            // What `brand_device_page` carries over to the branded page.
            assert_eq!(
                page.headers().get(name).map(HeaderValue::as_bytes),
                Some(value.as_bytes()),
                "{name}"
            );
        }
        let approval = brand_as_fetch("GET", "/v1/device-auth", page).await;
        assert!(approval.contains("Approve Living Brain CLI?"), "{approval}");
        assert!(approval.contains(&format!("<code>{code}</code>")));
        assert!(approval.contains(&format!(
            "<form method=\"post\" action=\"/v1/device-auth/approve\">\
             <input type=\"hidden\" name=\"user_code\" value=\"{code}\">"
        )));
        assert!(approval.contains(&format!(
            "<form method=\"post\" action=\"/v1/device-auth/deny\">\
             <input type=\"hidden\" name=\"user_code\" value=\"{code}\">"
        )));
        assert!(
            approval.contains("nick&#39;s &lt;mac&gt;"),
            "the label, escaped"
        );
        assert!(!approval.contains("<mac>") && !approval.contains("no scopes"));

        // The branded Approve button posts what the module's did.
        let decision = [
            ("sec-fetch-site", "same-origin"),
            ("origin", "https://app.livingbrain.wiki"),
            ("content-type", "application/x-www-form-urlencoded"),
        ];
        let body = format!("user_code={code}");
        let approved = send(
            &kit,
            HttpMethod::POST,
            "/v1/device-auth/approve",
            &decision,
            &body,
        )
        .await;
        assert_eq!(approved.status(), 200);
        let approved = brand_as_fetch("POST", "/v1/device-auth/approve", approved).await;
        assert!(approved.contains("You can close this tab and return to your terminal."));

        let denied_code = new_user_code(&kit).await;
        let body = format!("user_code={denied_code}");
        let denied = send(
            &kit,
            HttpMethod::POST,
            "/v1/device-auth/deny",
            &decision,
            &body,
        )
        .await;
        assert_eq!(denied.status(), 200);
        let denied = brand_as_fetch("POST", "/v1/device-auth/deny", denied).await;
        assert!(denied.contains("Request denied"));

        // An answered code: the module's problem+json, which is not HTML
        // and so is never branded.
        let again = send(
            &kit,
            HttpMethod::POST,
            "/v1/device-auth/approve",
            &decision,
            &body,
        )
        .await;
        let (content_type, _) = read(again).await;
        assert!(
            !device_pages::is_html(content_type.as_deref()),
            "{content_type:?}"
        );

        let form = send(&kit, HttpMethod::GET, "/v1/device-auth", &[], "").await;
        let form = brand_as_fetch("GET", "/v1/device-auth", form).await;
        assert!(
            form.contains("method=\"get\" action=\"/v1/device-auth\""),
            "{form}"
        );
        let unknown = send(
            &kit,
            HttpMethod::GET,
            "/v1/device-auth?user_code=ZZZZ-ZZZZ",
            &[],
            "",
        )
        .await;
        let unknown = brand_as_fetch("GET", "/v1/device-auth", unknown).await;
        assert!(unknown.contains("That code is not valid"), "{unknown}");

        // Previews for review, only when asked for: the pages as rendered,
        // with the deployed stylesheet's absolute URL so they open from disk.
        if let Some(dir) = std::env::var_os("LB_DEVICE_PAGES_PREVIEW") {
            preview(
                std::path::Path::new(&dir),
                &[
                    ("approve.html", &approval),
                    ("approved.html", &approved),
                    ("denied.html", &denied),
                    ("enter-code.html", &form),
                    ("unknown-code.html", &unknown),
                ],
            );
        }
    }

    /// Writes the pages to `dir` for a look in a browser. `app.css` and the
    /// favicon point at the deployed app; `device.css` is copied beside the
    /// pages, because until this change deploys the live app has none.
    fn preview(dir: &std::path::Path, pages: &[(&str, &String)]) {
        std::fs::create_dir_all(dir).expect("a preview directory");
        let app = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/assets");
        std::fs::copy(app.join("device.css"), dir.join("device.css")).expect("device.css");
        for (file, html) in pages {
            let html = html
                .replace("href=\"/assets/device.css\"", "href=\"device.css\"")
                .replace(
                    "href=\"/assets/",
                    "href=\"https://app.livingbrain.wiki/assets/",
                )
                .replace("href=\"/", "href=\"https://app.livingbrain.wiki/");
            std::fs::write(dir.join(file), html).expect("a preview page");
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

#[cfg(test)]
mod erasure_tests {
    //! ADR 0004, "Deleting a workspace": the seam an owner's deletion runs
    //! through shreds the workspace's own scopes and revokes its credentials,
    //! and leaves another workspace's untouched. Run over the migrated tables
    //! of every module whose rows the seam reaches.

    use super::*;
    use cratefield_core::{Blob, Clock, Database, Statement, UlidIdGen};
    use cratefield_kms::{Dek, LocalFileKms};
    use cratefield_testing::{MemoryBlob, TestHarness};
    use livingbrain_pages::{Author, EntityType, PageError, PageWrite};
    use sea_query::Value;

    const WORKSPACE: &str = "ws_DOOMED";
    const OTHER: &str = "ws_KEPT";
    const MEMBER: &str = "usr_SHARED_ID";

    fn kms() -> Arc<dyn Kms> {
        Arc::new(
            LocalFileKms::from_key(Dek::generate().expect("a key"), "test-kek", "test")
                .expect("a well-formed key"),
        )
    }

    fn harness() -> TestHarness {
        let modules: Vec<Box<dyn cratefield_core::Module>> = vec![
            Box::new(Pages::new()),
            Box::new(Tokens::new()),
            Box::new(Models::new()),
            Box::new(Tools::new()),
            Box::new(GithubApp::new()),
            Box::new(Access::new()),
        ];
        TestHarness::new(modules)
    }

    fn text(value: &str) -> Value {
        Value::String(Some(Box::new(value.to_owned())))
    }

    fn run(db: &Arc<dyn Database>, sql: &str, values: Vec<Value>) {
        pollster::block_on(db.execute(&Statement::with_values(sql, values))).expect("a statement");
    }

    fn count(db: &Arc<dyn Database>, sql: &str, values: Vec<Value>) -> usize {
        pollster::block_on(db.query(&Statement::with_values(sql, values)))
            .expect("a query")
            .rows
            .len()
    }

    async fn write(store: &PageStore, scope: &str, body: &str) {
        store
            .write(
                scope,
                "alpha",
                PageWrite {
                    entity_type: EntityType::Person,
                    markdown: format!("---\nname: Kestrel\n---\n{body}"),
                    author: Author::Human {
                        id: MEMBER.to_owned(),
                    },
                    base_version: None,
                },
            )
            .await
            .expect("a write");
    }

    #[test]
    fn erasing_a_workspace_shreds_its_scopes_and_revokes_its_credentials() {
        pollster::block_on(async {
            let kit = harness();
            let db = kit.db.clone();
            let blob: Arc<dyn Blob> = Arc::new(MemoryBlob::new());
            let clock: Arc<dyn Clock> = Arc::new(kit.clock.clone());
            let store = Arc::new(PageStore::new(
                db.clone(),
                blob,
                kms(),
                clock,
                Arc::new(UlidIdGen),
            ));

            let doomed_personal =
                livingbrain_pages::access::space_scope(WORKSPACE, "personal:usr_SHARED_ID")
                    .expect("a space");
            let kept_personal =
                livingbrain_pages::access::space_scope(OTHER, "personal:usr_SHARED_ID")
                    .expect("a space");
            let doomed_shared =
                livingbrain_pages::access::space_scope(WORKSPACE, "shared").expect("a space");
            let kept_shared =
                livingbrain_pages::access::space_scope(OTHER, "shared").expect("a space");
            // The same member id in both workspaces is two scopes: the
            // workspace is folded into the scope name.
            assert_ne!(doomed_personal, kept_personal);
            assert_ne!(doomed_shared, kept_shared);

            write(&store, &doomed_personal, "Zephyrmoor met in Reykjavik.").await;
            write(&store, &kept_personal, "Quicksilver ledger, Oslo.").await;
            write(&store, &doomed_shared, "Harbour notes for the doomed team.").await;
            write(&store, &kept_shared, "Harbour notes for the kept team.").await;

            // The rows the deletion reaches outside pages: a source in the
            // doomed scope, a PAT and a provider key in each workspace.
            run(
                &db,
                "INSERT INTO sources (scope, body_sha256, id, kind, rel_path, wikilinks, \
                 body_key, key_version, imported_by, created_at) \
                 VALUES (?, 'sha', 'src1', 'import', 'a.md', '[]', 'k', 1, ?, 'now')",
                vec![text(&doomed_personal), text(MEMBER)],
            );
            run(
                &db,
                "INSERT INTO sources (scope, body_sha256, id, kind, rel_path, wikilinks, \
                 body_key, key_version, imported_by, created_at) \
                 VALUES (?, 'sha', 'src2', 'import', 'b.md', '[]', 'k', 1, ?, 'now')",
                vec![text(&kept_personal), text(MEMBER)],
            );
            for (prefix, workspace) in [("lbp_doomed", WORKSPACE), ("lbp_kept", OTHER)] {
                run(
                    &db,
                    "INSERT INTO personal_access_tokens \
                     (prefix, secret_hash, workspace_id, user_id, created_at) \
                     VALUES (?, 'hash', ?, ?, 'now')",
                    vec![text(prefix), text(workspace), text(MEMBER)],
                );
                run(
                    &db,
                    "INSERT INTO model_connections (workspace_id, role, provider, base_url, \
                     model, key_ciphertext, key_last4, status, missing, checked_at, updated_by) \
                     VALUES (?, 'answer', 'openai', 'https://x', 'm', ?, '1234', 'ok', '', \
                     'now', ?)",
                    vec![text(workspace), b"sealed".to_vec().into(), text(MEMBER)],
                );
            }

            // Issues #51 and #146: what a connected app keeps for a subject,
            // in each workspace, and a published page in the doomed one. None
            // of it is a member's space, so the erasure finds it by its rows.
            let memories = livingbrain_pages::MemoryStore::new(
                db.clone(),
                kms(),
                Arc::new(kit.clock.clone()),
                Arc::new(UlidIdGen),
            );
            let doomed_subject = livingbrain_api::subject_scope(WORKSPACE, "earthos", "user:1");
            let kept_subject = livingbrain_api::subject_scope(OTHER, "earthos", "user:1");
            assert_ne!(doomed_subject, kept_subject);
            for (workspace, scope) in [(WORKSPACE, &doomed_subject), (OTHER, &kept_subject)] {
                memories
                    .note(workspace, "earthos", scope, "Likes sunrise walks", "conv")
                    .await
                    .expect("a memory");
            }
            let public_scope =
                livingbrain_pages::page_scope(WORKSPACE, &livingbrain_access::Scope::Public);
            write(&store, &public_scope, "Published refund policy.").await;
            run(
                &db,
                "INSERT INTO public_pages (scope, slug, workspace_id, title, url, source_scope, \
                 source_version, published_by, published_at) \
                 VALUES (?, 'alpha', ?, 'Alpha', 'https://x.test/a', 's', 1, 'owner', 'now')",
                vec![text(&public_scope), text(WORKSPACE)],
            );

            let erasure = WipeWorkspace {
                pages: PageStoreCell::default(),
            };
            // Before the pages surface has run, there is no store to shred
            // through, and the deletion must refuse rather than skip.
            let now = kit.clock.0;
            assert!(
                erasure
                    .erase(&*db, now, WORKSPACE, &["shared".to_owned()])
                    .await
                    .is_err(),
                "no page store means no erasure"
            );
            erasure.pages.publish(store.clone());

            let spaces = vec![
                "shared".to_owned(),
                format!("personal:{MEMBER}"),
                "channel:slack:C0PRIVATE".to_owned(),
            ];
            erasure
                .erase(&*db, now, WORKSPACE, &spaces)
                .await
                .expect("the erasure runs");

            // The doomed workspace's content is unreadable, and says so.
            for (scope, slug) in [(&doomed_personal, "alpha"), (&doomed_shared, "alpha")] {
                let error = store
                    .read(scope, slug)
                    .await
                    .expect_err("a shredded scope must not read");
                assert!(matches!(error, PageError::Shredded(_)), "{error:?}");
            }
            // The app memories and the public scope went with it: no row, no
            // key — and the other workspace's subject, same app and same
            // subject string, reads on.
            assert_eq!(
                count(
                    &db,
                    "SELECT id FROM app_memories WHERE scope = ?",
                    vec![text(&doomed_subject)]
                ),
                0
            );
            assert!(matches!(
                memories
                    .note(WORKSPACE, "earthos", &doomed_subject, "again", "")
                    .await,
                Err(livingbrain_pages::MemoryError::Store(PageError::Shredded(
                    _
                )))
            ));
            assert_eq!(
                memories
                    .recall(&kept_subject, 20)
                    .await
                    .expect("a recall")
                    .len(),
                1
            );
            assert!(matches!(
                store.read(&public_scope, "alpha").await,
                Err(PageError::Shredded(_))
            ));
            assert_eq!(
                count(
                    &db,
                    "SELECT slug FROM public_pages WHERE workspace_id = ?",
                    vec![text(WORKSPACE)]
                ),
                0
            );
            // The other workspace, with the same member id, reads on.
            assert!(store.read(&kept_personal, "alpha").await.is_ok());
            assert!(store.read(&kept_shared, "alpha").await.is_ok());

            // Its sources went with the scope; the other workspace's did not.
            assert_eq!(
                count(
                    &db,
                    "SELECT id FROM sources WHERE scope = ?",
                    vec![text(&doomed_personal)]
                ),
                0
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT id FROM sources WHERE scope = ?",
                    vec![text(&kept_personal)]
                ),
                1
            );

            // Its tokens are revoked, the other workspace's are not, and its
            // provider key is gone.
            assert_eq!(
                count(
                    &db,
                    "SELECT prefix FROM personal_access_tokens \
                     WHERE prefix = 'lbp_doomed' AND revoked_at IS NULL",
                    vec![],
                ),
                0
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT prefix FROM personal_access_tokens \
                     WHERE prefix = 'lbp_kept' AND revoked_at IS NULL",
                    vec![],
                ),
                1
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT workspace_id FROM model_connections WHERE workspace_id = ?",
                    vec![text(WORKSPACE)],
                ),
                0
            );
            assert_eq!(
                count(
                    &db,
                    "SELECT workspace_id FROM model_connections WHERE workspace_id = ?",
                    vec![text(OTHER)],
                ),
                1
            );

            // A space that is not a space is refused, not silently skipped.
            assert!(
                erasure
                    .erase(&*db, now, WORKSPACE, &["nonsense".to_owned()])
                    .await
                    .is_err()
            );
        });
    }
}

#[cfg(test)]
mod device_poll_tests {
    use super::is_device_poll;

    /// L4: the device grant's poll is recognised in both encodings a client may
    /// send, and nothing else is re-addressed.
    #[test]
    fn a_device_poll_is_recognised_and_nothing_else_is() {
        assert!(is_device_poll(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code=x&client_id=livingbrain-cli"
        ));
        assert!(is_device_poll(
            "device_code=x&grant_type=urn:ietf:params:oauth:grant-type:device_code"
        ));
        assert!(!is_device_poll("grant_type=authorization_code&code=x"));
        assert!(!is_device_poll("grant_type=refresh_token&refresh_token=x"));
        assert!(!is_device_poll(
            "note=grant_type=urn:ietf:params:oauth:grant-type:device_code"
        ));
    }
}
