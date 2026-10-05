//! Mints a `workspaces.session` cookie value and prints it, so the staging
//! smoke test can make one authenticated read without a Slack sign-in
//! (`scripts/deploy/smoke.sh`).
//!
//! It exists for the staging smoke test and nothing else. It signs a session
//! with a real `HARNESS_SECRET`, so it is a credential mint: never point it at
//! a production `HARNESS_SECRET` in casual use, and never run it with one
//! committed anywhere. The cookie is good for [`TTL_SECS`] and names the
//! workspace and member the smoke test seeded (`T0SMOKETEST`/`U0SMOKETEST` by
//! default), so it is only ever useful against a seeded staging database.
//!
//! ```sh
//! HARNESS_SECRET=... ENV=staging \
//!   cargo run -q -p livingbrain-workspaces --example mint_session
//! ```
//!
//! `ENV` and `HARNESS_VENTURE` must be the values the deployed Worker sees:
//! the signer binds every token to them as `{venture}|{env}`, and a token
//! whose binding does not match is refused.

use std::time::{SystemTime, UNIX_EPOCH};

use cratefield_core::{HarnessConfig, Kid, MapConfig, Payload, Signer};
use livingbrain_workspaces::SESSION_PURPOSE;
use serde_json::json;

/// The Slack team id of the seeded smoke-test workspace.
const DEFAULT_TEAM_ID: &str = "T0SMOKETEST";
/// The Slack user id of its one seeded member.
const DEFAULT_USER_ID: &str = "U0SMOKETEST";
/// Short on purpose: a smoke test runs once and the cookie is thrown away.
const TTL_SECS: u64 = 600;

fn main() {
    let secret = env_required("HARNESS_SECRET");
    let env = std::env::var("ENV").unwrap_or_else(|_| "staging".to_owned());
    let venture = std::env::var("HARNESS_VENTURE").ok();
    let team_id = arg_or_env(1, "TEAM_ID", DEFAULT_TEAM_ID);
    let user_id = arg_or_env(2, "USER_ID", DEFAULT_USER_ID);

    // The same parse the Worker's own runtime does at boot, so the key ring,
    // the binding and the policy are the deployed ones.
    let mut pairs = vec![("HARNESS_SECRET", secret), ("ENV", env.clone())];
    if let Some(venture) = venture.clone() {
        pairs.push(("HARNESS_VENTURE", venture));
    }
    let config = HarnessConfig::from_config(&MapConfig::from_pairs(pairs)).unwrap_or_else(|err| {
        // Named plainly, because the same problem would stop the deployed
        // Worker booting: a smoke test that cannot mint a cookie must say
        // the config is wrong rather than panic at a line number.
        eprintln!("the harness configuration is not valid: {err}");
        std::process::exit(2);
    });
    let signer = config.signer();

    let exp = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_secs()
            + TTL_SECS,
    )
    .expect("the expiry fits an i64");
    let subject = json!({"team_id": team_id, "user_id": user_id, "exp": exp});

    // `exp: None` on the payload: the session's own expiry is inside the
    // subject, which is where the module reads it from.
    print!(
        "{}",
        signer.sign(&Payload {
            purpose: SESSION_PURPOSE.to_owned(),
            subject: subject.to_string(),
            exp: None,
            kid: Kid::Cur,
        })
    );
}

/// A required variable, or a message naming it and a non-zero exit.
fn env_required(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.is_empty() => value,
        _ => {
            eprintln!("{name} must be set to the value the deployed Worker sees");
            std::process::exit(2);
        }
    }
}

/// The positional argument at `index`, else the environment variable `name`,
/// else `fallback`.
fn arg_or_env(index: usize, name: &str, fallback: &str) -> String {
    std::env::args()
        .nth(index)
        .or_else(|| std::env::var(name).ok())
        .unwrap_or_else(|| fallback.to_owned())
}
