//! The Living Brain Worker: the composition root, and the only crate that
//! names a runtime and a vendor SDK.
//!
//! It mounts every `livingbrain-*` module into one Cratefield harness and
//! serves it on Cloudflare Workers. The module set is the empty `canary`
//! (the scaffold) and the harness `waitlist` module — the early-access list
//! behind issue #23: joins from livingbrain.wiki, double opt-in mail through
//! Owlpost, and the CSV export behind the admin token. `wrangler.toml` is
//! beside this crate.
#![forbid(unsafe_code)]

use cratefield_adapter_owlpost::Owlpost;
use cratefield_adapter_turnstile::Turnstile;
use cratefield_core::{Harness, Mailer, Venture};
use cratefield_module_waitlist::{Waitlist, default_templates};
use cratefield_runtime_cloudflare::{
    Cloudflare, FetchClient, WorkersClock, serve, serve_scheduled,
};
use livingbrain_canary::Canary;
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

/// The Owlpost `Mailer`, read from the Worker `Env` — never `std::env`,
/// which is empty on Workers. A missing or blank `OWLPOST_API_KEY` leaves the
/// adapter `NotConfigured`: it still provides the port, and a join is
/// captured as a pending entry rather than failing. `OWLPOST_BASE_URL`
/// (optional) points at a self-hosted or proxied Owlpost.
fn build_mailer(env: &Env) -> Arc<dyn Mailer> {
    let key = env
        .secret("OWLPOST_API_KEY")
        .ok()
        .map(|secret| secret.to_string())
        .filter(|key| !key.is_empty());
    let mut mailer = Owlpost::new(
        Arc::new(FetchClient),
        Arc::new(WorkersClock),
        key,
        // The module sets each mail's real `From` (`no-reply@send.<domain>`);
        // this is only the adapter's fallback, so it never becomes the sender.
        "no-reply@send.livingbrain.wiki",
        None,
    );
    if let Ok(base) = env.var("OWLPOST_BASE_URL") {
        let base = base.to_string();
        if !base.is_empty() {
            mailer = mailer.with_base_url(base);
        }
    }
    Arc::new(mailer)
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

/// The composed harness and the runtime it was validated against.
///
/// One runtime, built once: `Harness::build` checks every module's
/// `requires()` against the ports of the instance it is handed, so a second,
/// separately-built instance would validate something other than what serves.
/// The same instance is cloned into the harness and handed to `serve`.
///
/// Secrets are read here from `env`, once, because `std::env` is empty on
/// Workers; `Env` is a per-request handle but the secrets behind it are
/// deployment-wide.
fn instance(env: &Env) -> &'static (Harness, Cloudflare) {
    INSTANCE.get_or_init(|| {
        let runtime = Cloudflare::new()
            // The bindings `wrangler.toml` declares: D1 (R2 and KV are wired
            // so the deployment shape is exercised even before a module
            // resolves through them), the Owlpost mailer, and the Workers
            // Rate Limiting binding the `waitlist` module's public write and
            // admin routes are guarded by (issue #437). The `waitlist`
            // module's `Signer` port needs no line here: the runtime resolves
            // it from `HARNESS_SECRET`.
            .db("DB")
            .blob("R2")
            .kv("KV")
            .mailer_arc(build_mailer(env))
            .rate_limiter("RATE_LIMITER");
        let runtime = match build_captcha(env) {
            Some(captcha) => runtime.captcha(captcha),
            None => runtime,
        };
        let harness = Harness::builder()
            .venture(venture())
            // The waitlist module's askama templates (issue #23), so the
            // confirmation mail renders even before any venture override.
            .templates(default_templates())
            .module(Canary::new())
            .module(waitlist_module())
            .runtime(runtime.clone())
            .build()
            .expect("the livingbrain harness is valid");
        (harness, runtime)
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
