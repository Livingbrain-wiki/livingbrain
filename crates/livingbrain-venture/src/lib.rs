//! The Living Brain Worker: the composition root, and the only crate that
//! names a runtime and a vendor SDK.
//!
//! It mounts every `livingbrain-*` module into one Cratefield harness and
//! serves it on Cloudflare Workers. Today the module set is the single empty
//! `canary` module, so the Worker answers the harness's own `/__health` and
//! nothing else — the scaffold proven end to end before any product code
//! exists to blame. `wrangler.toml` is beside this crate.
#![forbid(unsafe_code)]

use cratefield_core::{Harness, Venture};
use cratefield_runtime_cloudflare::{Cloudflare, serve};
use livingbrain_canary::Canary;
use livingbrain_pages::Pages;
use std::sync::OnceLock;
use worker::{Context, Env, Request, Response, event};

static INSTANCE: OnceLock<(Harness, Cloudflare)> = OnceLock::new();

/// The composed harness and the runtime it was validated against.
///
/// One runtime, built once: `Harness::build` checks every module's
/// `requires()` against the ports of the instance it is handed, so a second,
/// separately-built instance would validate something other than what serves.
/// The same instance is cloned into the harness and handed to `serve`.
fn instance() -> &'static (Harness, Cloudflare) {
    INSTANCE.get_or_init(|| {
        let runtime = Cloudflare::new()
            // The three bindings `wrangler.toml` declares. The canary requires
            // no port, so nothing resolves through them yet; they are wired
            // here so the deployment shape (D1, R2, KV) is exercised from the
            // first commit and the first real module finds its ports already
            // in place.
            .db("DB")
            .blob("R2")
            .kv("KV");
        let harness = Harness::builder()
            .venture(
                Venture::new("livingbrain", "api.livingbrain.wiki")
                    .public_url("https://api.livingbrain.wiki")
                    .cors_origins(["https://livingbrain.wiki", "https://api.livingbrain.wiki"]),
            )
            .module(Canary::new())
            .module(Pages::new())
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
    let (harness, runtime) = instance();
    serve(harness, runtime, req, env, ctx).await
}
