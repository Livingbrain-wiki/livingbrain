//! `livingbrain-workspaces`: one tenant per chat workspace, with Slack and an
//! email magic link as equal ways in.
//!
//! The module owns the tables that make a workspace a tenant, the sign-in
//! that creates them, and the member mirror. The first person to sign in owns
//! the workspace, and that is the only moment an owner is ever decided.
//!
//! ADR 0002 supersedes ADR 0001's "workspace id = Slack team id": ids are
//! now opaque and ours to mint, Slack and Discord are *linked connections*,
//! and people are *linked identities* inside a workspace. A workspace
//! therefore does not need Slack at all — `POST /email/start` creates one.
//!
//! Isolation is enforced here, not by the deployment: one D1 holds every
//! workspace, every row is scoped by a workspace id, and that id always
//! comes from the verified session cookie, from a Slack `id_token` this
//! server asked for, or from a spent single-use sign-in token — never from
//! request input. The ADR records why, and when to revisit it.
//!
//! [`apply_user_change`] is the seam issue #6's Events webhook calls to
//! refresh the member mirror, after it has verified the event's signature
//! and read the team id out of the signed envelope.
//!
//! [`caller`] is the same kind of seam for a sibling module (issue #10's
//! `models`): who is signed in, read through this module's own session
//! check rather than a second implementation of it.
//!
//! [`link_connection`] and [`link_identity`] are the same kind of seam for
//! issue #56: a caller that has verified a Discord guild or user id binds it
//! to a workspace and a member. Discord's OAuth route is not here yet.
//!
//! [`Answers`] is the same kind of seam for issue #123, in the other
//! direction: this module owns the Slack conversation and cannot read a page,
//! so a composition injects something that can, and the agent loop answers
//! through it. [`Turns`] is its sibling for issue #7: several people can
//! talk in one thread at once, and what serialises the agent's turns is a
//! per-conversation coordinator only the composition can provide.

#![forbid(unsafe_code)]

mod agent;
mod config;
mod conversation;
mod events;
mod flow;
mod handlers;
mod install;
mod reply;
mod slack;
mod store;

pub use config::Settings;
pub use conversation::{
    Admit, Answer, Call, Conversation, Exchange, LocalTurns, Pending, Room, Turn, TurnError, Turns,
    apply,
};
pub use events::bot_scopes;
pub use flow::{FLOW_COOKIE, FLOW_PURPOSE, SESSION_COOKIE, SESSION_PURPOSE};
pub use handlers::{Caller, caller, caller_for};
pub use install::{BotTokenError, bot_token};
pub use livingbrain_pages::Answers;
pub use store::{LinkOutcome, UserChange, apply_user_change, link_connection, link_identity};

use std::sync::Arc;

use cratefield_core::{
    Config, ConfigError, DataKind, Disposition, Migrations, Module, ModuleContext, PersonalDataSet,
    Port, SqlMigration,
};

/// The first migration: `workspaces` and `workspace_members`.
const MIGRATION_INIT: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/sqlite/0001_init.sql"),
);

/// The Postgres form of [`MIGRATION_INIT`]. The schema is the portable
/// subset (ADR 0004), so the SQL is the sqlite file's unchanged; the set
/// is shipped anyway so the parity job applies a Postgres migration rather
/// than falling back to the sqlite one.
const MIGRATION_INIT_POSTGRES: SqlMigration = SqlMigration::new(
    "0001",
    "init",
    include_str!("../migrations/postgres/0001_init.sql"),
);

/// The second migration: `workspace_connections`, `member_identities` and
/// `sign_in_links` (issue #71). It backfills a Slack connection and a Slack
/// identity for every row `0001` created, so nothing that exists changes.
const MIGRATION_0002: SqlMigration = SqlMigration::new(
    "0002",
    "identities",
    include_str!("../migrations/sqlite/0002_identities.sql"),
);

/// The Postgres form of [`MIGRATION_0002`], the same file byte for byte:
/// `0002` uses only the portable subset (TEXT columns, no dialect type, no
/// `AUTOINCREMENT`), which is what makes one file serve both dialects.
const MIGRATION_0002_POSTGRES: SqlMigration = SqlMigration::new(
    "0002",
    "identities",
    include_str!("../migrations/postgres/0002_identities.sql"),
);

/// The third migration: `workspaces_slack_inbox` and `slack_installs`
/// (issue #6). The inbox is the dedup ledger Slack's retries are claimed in;
/// the installs table holds one sealed bot token per team. Both are additive,
/// so a deployment with no Slack app never writes either.
const MIGRATION_0003: SqlMigration = SqlMigration::new(
    "0003",
    "slack_app",
    include_str!("../migrations/sqlite/0003_slack_app.sql"),
);

/// The Postgres form of [`MIGRATION_0003`], the same file byte for byte.
const MIGRATION_0003_POSTGRES: SqlMigration = SqlMigration::new(
    "0003",
    "slack_app",
    include_str!("../migrations/postgres/0003_slack_app.sql"),
);

/// Workspaces, Slack sign-in and the member mirror.
#[derive(Default)]
pub struct Workspaces {
    /// The seam the Slack agent answers through (issue #123). A composition
    /// that leaves it `None` has a workspaces module and no agent.
    answers: Option<Arc<dyn Answers>>,
    /// The seam the agent takes its turn from (issue #7). `None` is the
    /// per-isolate [`LocalTurns`]: right for a single isolate, and what the
    /// venture replaces with its per-conversation Durable Object.
    turns: Option<Arc<dyn Turns>>,
}

// The two seams are `dyn`s and not a `Debug`; the module itself is still
// one, so a composition that prints its modules keeps working.
impl std::fmt::Debug for Workspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspaces")
            .field("answers", &self.answers.is_some())
            .field("turns", &self.turns.is_some())
            .finish()
    }
}

impl Workspaces {
    /// A new workspaces module. There is nothing to configure in the
    /// builder; the Slack app settings come from config at router build.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Answer @mentions and DMs from `answers` (issue #123).
    ///
    /// The seam is injected rather than built here because a `PageStore` is a
    /// tenant-wide capability and this module may not hold one: what it may
    /// hold is "a way to answer somebody, given their workspace and their
    /// member id", which is exactly what [`Answers`] is.
    #[must_use]
    pub fn answering(mut self, answers: Arc<dyn Answers>) -> Self {
        self.answers = Some(answers);
        self
    }

    /// Take the agent's turns from `turns` (issue #7).
    ///
    /// The seam is injected rather than built here because the coordinator
    /// is a Durable Object keyed by the conversation, and a Durable Object
    /// class can only be declared by the crate compiled into the Worker —
    /// the same constraint [`Answers`] is injected for, with a different
    /// answer behind it: what this module may hold is "a way to learn
    /// whether a message in this conversation answers now or queues", which
    /// is exactly what [`Turns`] is.
    #[must_use]
    pub fn taking_turns(mut self, turns: Arc<dyn Turns>) -> Self {
        self.turns = Some(turns);
        self
    }
}

impl Module for Workspaces {
    fn name(&self) -> &'static str {
        "workspaces"
    }

    fn version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// The ports the handlers reach. `Signer` seals the flow and session
    /// cookies, `HttpClient` talks to Slack's token endpoint, `Clock`
    /// decides expiry, `IdGen` mints the CSRF state, the nonce, the
    /// workspace and member ids and the sign-in token, `Defer` carries an
    /// events delivery's work past Slack's three-second acknowledgement, and
    /// the database holds the tables.
    fn requires(&self) -> &'static [Port] {
        &[
            Port::Db,
            Port::Signer,
            Port::HttpClient,
            Port::Clock,
            Port::IdGen,
            Port::Defer,
        ]
    }

    /// `Mailer` is optional rather than required because ADR 0002 wants a
    /// workspace that needs no Slack and no mail: the deployment installs
    /// whatever sign-in methods it has, and the email routes answer `503`
    /// per request when no mailer is wired, exactly as the Slack routes
    /// answer `503` when the app keys are absent.
    ///
    /// `Classifier` is optional for the same reason, and for one more
    /// (issue #123): the Slack agent triages a DM with the fast judge
    /// (issue #112) and a deployment with no classifier has no judge — so it
    /// answers an @mention, which is a request, and stays out of a DM, which
    /// is a conversation. A deployment that wires one gets both gated by it.
    fn optional(&self) -> &'static [Port] {
        &[Port::Mailer, Port::Classifier]
    }

    fn tables(&self) -> &'static [&'static str] {
        &[
            "workspaces",
            "workspace_members",
            "workspace_connections",
            "member_identities",
            "sign_in_links",
            "workspaces_slack_inbox",
            "slack_installs",
        ]
    }

    /// Every table holds a Slack id for a person, and since issue #71 an
    /// email address too. `workspaces` names the owner, `workspace_members`
    /// names every member with the display name and timezone Slack reports,
    /// `member_identities` is the address-per-platform indirection, and
    /// `sign_in_links` is the single-use sign-in token's row.
    fn personal_data(&self) -> &'static [PersonalDataSet] {
        const SETS: &[PersonalDataSet] = &[
            PersonalDataSet {
                table: "workspaces",
                subject: "owner_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "The workspace's id and name, and the id of the person who \
                              first signed in — the workspace owner.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "workspace_members",
                subject: "user_id",
                kind: DataKind::Contact,
                disposition: Disposition::Erase,
                description: "For each workspace you signed in to: the member id your \
                              sign-in was issued, the display name the sign-in method \
                              reports for you, your timezone and whether Slack marks \
                              you an admin.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "member_identities",
                subject: "user_id",
                kind: DataKind::Identifier,
                disposition: Disposition::Erase,
                description: "For each workspace you belong to: the id Slack, Discord or \
                              your email address gives you, and the member id it resolves \
                              to inside that workspace.",
                redacted: &[],
                subject_via: None,
            },
            PersonalDataSet {
                table: "sign_in_links",
                subject: "email",
                kind: DataKind::Contact,
                disposition: Disposition::Erase,
                description: "Your email address and the workspace a sign-in link for it \
                              was minted against, kept for as long as the link can be \
                              spent. The link itself is stored only as a SHA-256 hash.",
                redacted: &["token_hash"],
                subject_via: None,
            },
            PersonalDataSet {
                table: "workspace_connections",
                subject: "external_id",
                kind: DataKind::Contact,
                disposition: Disposition::Erase,
                description: "For each workspace you signed in to with only an email \
                              address: that address, as the workspace's connection to \
                              itself. A Slack team or Discord guild is named here too, \
                              and that is the tenant rather than a person — the people \
                              inside a workspace are the rows of `workspace_members` \
                              and `member_identities`.",
                redacted: &[],
                subject_via: None,
            },
            // The two tables issue #6 added. Neither names a person: the
            // inbox is a dedup ledger of Slack's own event ids, and
            // `slack_installs` is one row per *workspace* holding a bot
            // credential sealed under a KMS-wrapped key whose AAD binds it to
            // the team, so no column here can yield anybody's Slack session.
            PersonalDataSet::none(
                "workspaces_slack_inbox",
                "the key is Slack's own `event_id` and the row holds nothing \
                 else but when it was seen",
            ),
            PersonalDataSet::none(
                "slack_installs",
                "the row is one workspace's bot installation: a team id, the \
                 app and bot user ids, and a bot token sealed under a \
                 KMS-wrapped key with the team as its AAD",
            ),
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 3] = [MIGRATION_INIT, MIGRATION_0002, MIGRATION_0003];
        // The array is the apply order; this refuses a gap, a duplicate or
        // an entry out of order at build time.
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 3] = [
            MIGRATION_INIT_POSTGRES,
            MIGRATION_0002_POSTGRES,
            MIGRATION_0003_POSTGRES,
        ];
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS_POSTGRES);
        Migrations {
            sqlite: &MIGRATIONS,
            postgres: &MIGRATIONS_POSTGRES,
        }
    }

    /// Reports the Slack app keys a deployment that *has* Slack must set.
    /// This is `fz doctor`'s half; the runtime half is the 503 the Slack
    /// routes answer when the keys are absent, because the router must
    /// still install.
    ///
    /// ADR 0002: Slack is optional. A deployment that configures neither
    /// Slack credential has no Slack app and must not be failed for it —
    /// the email routes are a complete sign-in on their own, and they need
    /// only `REDIRECT_BASE`, which the email link is built from. Once
    /// *either* credential is present the trio is required, so a
    /// half-configured app is still reported rather than running with a
    /// callback that cannot verify anything.
    fn validate_config(&self, cfg: &dyn Config) -> Result<(), ConfigError> {
        let module = cratefield_core::ModuleConfig::new(self.name(), cfg);
        let slack_configured = ["SLACK_CLIENT_ID", "SLACK_CLIENT_SECRET"]
            .iter()
            .any(|suffix| {
                module
                    .get_opt(suffix)
                    .is_some_and(|raw| !raw.trim().is_empty())
            });
        if !slack_configured {
            return Ok(());
        }
        Settings::from_config(cfg).map(|_| ())
    }

    fn router(&self, ctx: ModuleContext) -> cratefield_core::axum::Router {
        handlers::router(ctx, self.answers.clone(), self.turns.clone())
    }
}
