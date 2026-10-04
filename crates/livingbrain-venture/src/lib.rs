//! The Living Brain Worker: the composition root, and the only crate that
//! names a runtime and a vendor SDK.
//!
//! It mounts every `livingbrain-*` module into one Cratefield harness and
//! serves it on Cloudflare Workers: the empty `canary` module from the
//! scaffold, and `workspaces` — one tenant per Slack workspace, sign in with
//! Slack, and the member mirror the Events webhook (issue #6) refreshes.
//! `wrangler.toml` and the D1 migrations are beside this crate.
//!
//! `workspaces` requires `Db`, `Signer`, `HttpClient`, `Clock` and `IdGen`.
//! The runtime provides the last four unconditionally (`Signer` from
//! `HARNESS_SECRET`) and `Db` from the `.db("DB")` binding below, so
//! `Harness::build` refuses a composition that leaves any of them unwired.
#![forbid(unsafe_code)]

use cratefield_core::{Harness, Venture};
use cratefield_runtime_cloudflare::{Cloudflare, serve};
use livingbrain_canary::Canary;
use livingbrain_workspaces::Workspaces;
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
            // The three bindings `wrangler.toml` declares. `workspaces` reads
            // its two tables through the D1 binding; the canary requires no
            // port, so R2 and KV are still wired ahead of a module that needs
            // them — the deployment shape (D1, R2, KV) from the scaffold.
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
            .module(Workspaces::new())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `instance()` panics if the composition is invalid, so reaching past
    /// it is the assertion: `workspaces` requires `Db`, `Signer`,
    /// `HttpClient`, `Clock` and `IdGen`, and `Harness::build` refuses a
    /// runtime that does not provide one of them. Natively, where the
    /// Worker's bindings do not exist, `provides()` is still the static set
    /// the build checks — so this catches a module whose ports were never
    /// wired long before a deploy would.
    #[test]
    fn the_composition_provides_every_port_its_modules_require() {
        let (harness, _) = instance();
        let names: Vec<&str> = harness
            .modules()
            .iter()
            .map(|module| module.name())
            .collect();
        assert!(names.contains(&"canary"), "{names:?}");
        assert!(names.contains(&"workspaces"), "{names:?}");
    }
}
