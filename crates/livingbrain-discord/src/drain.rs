//! The in-Worker cron drain, and the only place an import may start.
//!
//! **Backfill and import run only as a system inside Living Brain**: the
//! Worker's scheduled handler reads each connected server's channels and files
//! their days. No route, agent, CLI command or script drives an import.
//!
//! The rule is structural as in `livingbrain-github-app`. Every progress write,
//! lease and day filing takes a [`Drain`], and a `Drain` can only be made by
//! [`run`], the body of [`crate::DiscordModule`]'s `scheduled`. Its constructor
//! is private to this file, and a test fails the build if any other source
//! file names it.

use cratefield_core::ModuleContext;

use crate::common::StoreError;
use crate::{Shared, state};

/// The capability to read Discord history and file it. Held only by the
/// scheduled drain: see the module docs.
#[derive(Debug)]
pub struct Drain {
    _private: (),
}

impl Drain {
    /// The one constructor. Called from [`run`] and from this crate's tests.
    fn for_scheduled_run() -> Self {
        Self { _private: () }
    }
}

/// The scheduled entry point. It mints the [`Drain`], forgets expired connect
/// states, then hands the drain to the tick in `sync.rs`.
pub(crate) async fn run(
    ctx: &ModuleContext,
    _cron: &str,
    shared: &Shared,
) -> Result<(), StoreError> {
    let (Some(db), Some(clock)) = (ctx.ports.db.clone(), ctx.ports.clock.clone()) else {
        return Ok(());
    };
    let drain = Drain::for_scheduled_run();
    state::prune(&*db, clock.now()).await?;
    crate::sync::run(ctx, &drain, shared).await
}
