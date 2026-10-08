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
//! opt-in mail through Owlpost, and the CSV export behind the admin token.
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
use cratefield_core::axum::http::{HeaderMap, HeaderValue, header};
use cratefield_core::{Harness, Ports, Template, Venture};
use cratefield_kms::{Kms, WorkerSecretKms};
use cratefield_mail_templates::MailTheme;
use cratefield_module_waitlist::{Waitlist, themed_templates};
use cratefield_runtime_cloudflare::{
    Cloudflare, FetchClient, WorkersClock, serve, serve_scheduled,
};
use livingbrain_canary::Canary;
use livingbrain_mcp::{Asker, AuthError, BearerAuth};
use livingbrain_models::Models;
use livingbrain_pages::Pages;
use livingbrain_workspaces::{SESSION_COOKIE, Workspaces};
use std::sync::{Arc, OnceLock};
use worker::{Context, Env, Request, Response, event};

/// The product slug the site's waitlist form joins (issue #23).
pub const WAITLIST_PRODUCT: &str = "livingbrain";

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
        .public_url("https://api.livingbrain.wiki")
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

static INSTANCE: OnceLock<(Harness, Cloudflare)> = OnceLock::new();

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
) -> (Harness, Cloudflare) {
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
        ));
    let runtime = match captcha {
        Some(captcha) => runtime.captcha(captcha),
        None => runtime,
    };
    let harness = Harness::builder()
        .venture(venture())
        // The waitlist module's confirm mails in this venture's theme.
        .templates(templates())
        .module(Canary::new())
        .module(Workspaces::new())
        .module(Models::new())
        .module(pages_module(kms))
        .module(waitlist_module())
        .runtime(runtime.clone())
        .build()
        .expect("the livingbrain harness is valid");
    (harness, runtime)
}

/// The pages module as this venture composes it, with the MCP server and the
/// source importer nested in it (issues #24 and #81): the harness scopes
/// `Blob` per module name, so only a surface built from the pages module's own
/// context can open a body those pages write.
///
/// With no key custodian there is no body anyone may open, so neither surface
/// is mounted at all and every other route keeps working — a deployment
/// missing a secret gets a working venture, not a Worker that panics on its
/// first request.
fn pages_module(kms: Option<Arc<dyn Kms>>) -> Pages {
    let Some(kms) = kms else {
        return Pages::new();
    };
    // Each surface gets its own handle to the same custodian and the same
    // credential resolver: one key, so a body either of them seals is a body
    // the other can open.
    let auth: Arc<dyn BearerAuth> = Arc::new(SessionBearer);
    let mcp_kms = Arc::clone(&kms);
    let mcp_auth = Arc::clone(&auth);
    Pages::new()
        .nest("/mcp", move |ctx| {
            livingbrain_mcp::router(ctx, Arc::clone(&mcp_kms), Arc::clone(&mcp_auth))
        })
        .nest("/sources", move |ctx| {
            livingbrain_mcp::sources_router(ctx, Arc::clone(&kms), Arc::clone(&auth))
        })
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

/// The interim [`BearerAuth`] for the MCP endpoint (issue #24): the bearer
/// value **is** the session cookie value the web app already issues, and it
/// is verified through the `workspaces` module's own `caller` — the same check
/// that module's routes make, so there is one implementation of "who is
/// signed in" rather than two that can drift.
///
/// The MCP endpoint is not the web app and a browser cookie jar is not an MCP
/// client, so a user pastes their cookie value into their agent's config. That
/// is a stand-in, and docs/mcp.md says so; issue #72 replaces this
/// implementation with a real OAuth token and leaves the trait alone.
struct SessionBearer;

/// RFC 6265 §4.1.1 `cookie-octet`: US-ASCII except the separators and the
/// controls. A session token is base64url, so it never needs one of these.
fn is_cookie_octet(byte: u8) -> bool {
    matches!(byte,
        b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z'
        | b'!' | b'#'..=b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}

#[async_trait::async_trait]
impl BearerAuth for SessionBearer {
    async fn authenticate(&self, ports: &Ports, token: &str) -> Result<Asker, AuthError> {
        let (Some(signer), Some(clock), Some(db)) =
            (ports.signer.clone(), ports.clock.clone(), ports.db.clone())
        else {
            return Err(AuthError::Rejected);
        };
        // The token is spliced into a `Cookie` header, so anything outside
        // RFC 6265's cookie-octet set is refused here rather than splitting
        // the cookie, or smuggling a second one, on the way in.
        if !token.bytes().all(is_cookie_octet) {
            return Err(AuthError::Rejected);
        }
        let Ok(value) = HeaderValue::from_str(&format!("{SESSION_COOKIE}={token}")) else {
            return Err(AuthError::Rejected);
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, value);
        livingbrain_workspaces::caller(&*signer, &*clock, &*db, &headers)
            .await
            .map(|caller| Asker {
                workspace_id: caller.workspace_id,
                user_id: caller.user_id,
            })
            .map_err(|_| AuthError::Rejected)
    }
}

/// The process-wide instance, built on the first request from that request's
/// `Env`. Worker bindings are static for a deployment, so the first `Env` is
/// every `Env`; secrets are read from it because `std::env` is empty on
/// Workers.
fn instance(env: &Env) -> &'static (Harness, Cloudflare) {
    INSTANCE.get_or_init(|| {
        build(
            MailSettings::from_env(env),
            build_captcha(env),
            build_kms(env),
        )
    })
}

#[event(fetch)]
/// Worker fetch entry point.
///
/// # Errors
///
/// Propagates `worker::Error` from the harness router.
pub async fn fetch(req: Request, env: Env, ctx: Context) -> worker::Result<Response> {
    let (harness, runtime) = instance(&env);
    serve(harness, runtime, req, env, ctx).await
}

#[event(scheduled)]
/// Worker scheduled (cron) entry point: fans a firing out over the same
/// composed harness's modules — the `waitlist` module prunes expired
/// confirmation claims here. The cron lives in `wrangler.toml`; a trigger on
/// another Worker never reaches this handler.
pub async fn scheduled(event: worker::ScheduledEvent, env: Env, ctx: worker::ScheduleContext) {
    let (harness, runtime) = instance(&env);
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
        let (harness, _) = build(unconfigured_mail(), None, Some(Arc::new(kek)));
        let names: Vec<&str> = harness
            .modules()
            .iter()
            .map(|module| module.name())
            .collect();
        assert!(names.contains(&"canary"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
        assert!(names.contains(&"models"), "{names:?}");
        assert!(names.contains(&"pages"), "{names:?}");
        assert!(names.contains(&"waitlist"), "{names:?}");
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

    /// A deployment with no page key ring still composes: the MCP surface is
    /// not mounted and every other route keeps working.
    #[test]
    fn a_missing_key_ring_leaves_the_rest_of_the_composition_serving() {
        let (harness, _) = build(unconfigured_mail(), None, None);
        let names: Vec<&str> = harness.modules().iter().map(|m| m.name()).collect();
        assert!(names.contains(&"pages"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
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
