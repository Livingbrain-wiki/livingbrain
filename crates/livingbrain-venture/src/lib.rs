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
//! also what the MCP endpoint now accepts.
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
use cratefield_core::axum::body::{Body, to_bytes};
use cratefield_core::axum::http::{
    HeaderMap, HeaderValue, Method as HttpMethod, Request as HttpRequest, header,
};
use cratefield_core::axum::response::Response as AxumResponse;
use cratefield_core::{
    Blob, Classifier, Database, Harness, HarnessBuilder, Module, Ports, Problem, RandomBytes,
    RandomError, Rendered, ScheduledBudget, ScheduledLimits, ScheduledSplit, Template, Venture,
};
use cratefield_kms::{Kms, WorkerSecretKms};
use cratefield_mail_templates::{MailTheme, Message as MailBody};
use cratefield_module_device_auth::{DeviceAuth, DeviceClient};
use cratefield_module_waitlist::{Waitlist, themed_templates};
use cratefield_runtime_cloudflare::{
    Cloudflare, ContextDefer, FetchClient, ScheduleDefer, WorkersClock, install_tracing, serve,
};
use livingbrain_accounts::{Access, Agents, Audit};
use livingbrain_api::{
    Ask, BudgetSettings, CodingAgents, Export, Facts, Graph, ModelJobs, Notes, PagesRebind, Search,
    SessionAuth, Sources, Wiki,
};
use livingbrain_canary::Canary;
use livingbrain_github_app::GithubApp;
use livingbrain_mcp::{Asker, AuthError, BearerAuth};
use livingbrain_models::Models;
use livingbrain_models::jev::{JevConnector, JevKey};
use livingbrain_pages::{Answered, Answers, PageAnswers, PageStore, Pages};
use livingbrain_tokens::{Authenticated, Avatars, DevicePorts, Me, Tokens};
use livingbrain_tools::Tools;
use livingbrain_usage::Usage;
use livingbrain_workspaces::{
    Admit, AvatarRebind, EmailLedger, Exchange, LocalTurns, MemberRemoval, Pending, SignInMail,
    SignInMailInput, Turn, TurnError, Turns, Workspaces,
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
                    .on_member_removed(Arc::new(RevokeOnRemoval)),
            )
            .module(match &jev {
                Some(jev) => Models::new().with_jev(jev.clone()),
                None => Models::new(),
            })
            .module(Tools::new())
            .module(device_auth_module(&device_ports))
            .module(tokens_module(device_ports))
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
            .module(pages_module(kms.clone(), &answers))
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
    let wiki = wiki(kms).with_jev(jev);
    vec![
        Arc::new(Notes::new(wiki.clone())),
        Arc::new(Search::new(wiki.clone())),
        Arc::new(Ask::new(wiki.clone())),
        Arc::new(Export::new(wiki.clone())),
        Arc::new(Facts::new(wiki.clone())),
        Arc::new(Sources::new(wiki.clone())),
        // The brain map's data (`GET /v1/graph`): the asker's own pages,
        // sources and facts as nodes and edges, read-only.
        Arc::new(Graph::new(wiki.clone())),
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
    // The page routes the web app reads and writes answer to its session
    // cookie as well as to a bearer; the MCP endpoint and the importer
    // below take the bearer resolver alone and stay bearer-only.
    let surface = wiki(Arc::clone(&kms));
    let cell = answers.clone();
    Pages::new()
        .surface(move |ctx| livingbrain_api::pages_routes(ctx, &surface))
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
                    // Where a citation opens: this deployment's own web app
                    // (`WORKSPACES_REDIRECT_BASE`), the origin every other
                    // surface cites too — not the marketing site.
                    livingbrain_pages::app_base(ctx.config.as_ref()),
                )));
            }
            livingbrain_mcp::router(ctx, Arc::clone(&mcp_kms), Arc::clone(&mcp_auth))
        })
        .nest("/sources", move |ctx| {
            livingbrain_mcp::sources_router(ctx, Arc::clone(&kms), Arc::clone(&auth))
        })
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
        match livingbrain_tokens::authenticate_any(ports, &headers).await {
            Ok(Authenticated::Member(member)) => Ok(member_asker(member)),
            Ok(Authenticated::Agent(agent)) => Ok(Asker {
                workspace_id: agent.workspace_id,
                user_id: agent.agent_id,
                token_scopes: agent.scopes,
                agent_owner: Some(agent.owner_id),
                role: None,
                credential: Some(agent.credential),
            }),
            Err(_) => Err(AuthError::Rejected),
        }
    }
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
const CLI_ROUTES: [&str; 9] = [
    "/v1/notes",
    "/v1/search",
    "/v1/graph",
    "/v1/ask",
    "/v1/export",
    "/v1/sources",
    "/v1/coding-agents",
    "/v1/facts",
    "/v1/model-jobs",
];

/// The diverted paths that have routes *under* them, so the divert matches
/// the prefix: the sources listing, facet and citation route, the
/// coding-agents queue and process routes, and a fact's retraction
/// (`/v1/facts/{id}/retract`).
const CLI_PREFIXES: [&str; 4] = [
    "/v1/sources",
    "/v1/coding-agents",
    "/v1/facts",
    "/v1/model-jobs",
];

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
/// generous ceiling for what the CLI routes answer (JSON, one zip).
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
    }
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
        for module in ["notes", "search", "ask", "export", "sources", "facts"] {
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
