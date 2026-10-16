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
//!
//! [`SignInMail`] is one more seam of the same kind: the venture's mail theme
//! and template crate are the composition's, so the composition renders the
//! sign-in mail and this module keeps what makes it safe to send — the link,
//! its lifetime and the per-link idempotency key.

#![forbid(unsafe_code)]

mod agent;
mod config;
mod conversation;
mod events;
mod flow;
mod handlers;
mod install;
mod members;
mod profile;
mod reply;
mod slack;
mod store;
mod turn;

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
pub use profile::{
    AVATAR_BODY_CEILING, AVATAR_CACHE_CONTROL, AVATAR_MAX_BYTES, AVATAR_MAX_SIDE, AVATAR_PREFIX,
    AvatarRebind, AvatarRef, CleanImage, ImageError, ImageKind, NAME_MAX_CHARS, NameError,
    avatar_key, avatar_url, avatars_unavailable, blob_failed, clean_image, clean_name,
    forget_avatar, image_problem, is_avatar_id, member_avatar, name_problem, new_avatar_id,
    nothing_to_change, readable_avatar, set_display_name, set_member_avatar, store_avatar,
};
pub use store::{LinkOutcome, Role, UserChange, apply_user_change, link_connection, link_identity};
pub use turn::set_proactivity;

use std::sync::Arc;

use cratefield_core::{
    Config, ConfigError, DataKind, Database, Disposition, Migrations, Module, ModuleContext,
    PersonalDataSet, Port, SqlMigration,
};
use time::OffsetDateTime;

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

/// The fourth migration: the speak-up policy (`turn_policies`) and the
/// triage cost ledger (`turn_triage`) behind it (issue #9). Both are
/// additive; a deployment where nobody has ever set a policy writes nothing.
const MIGRATION_0004: SqlMigration = SqlMigration::new(
    "0004",
    "turn",
    include_str!("../migrations/sqlite/0004_turn.sql"),
);

/// The Postgres form of [`MIGRATION_0004`], the same file byte for byte.
const MIGRATION_0004_POSTGRES: SqlMigration = SqlMigration::new(
    "0004",
    "turn",
    include_str!("../migrations/postgres/0004_turn.sql"),
);

/// Members, roles and invitations (ADR 0004): the `role` and `removed_at`
/// columns and the invitations table.
const MIGRATION_0005: SqlMigration = SqlMigration::new(
    "0005",
    "members",
    include_str!("../migrations/sqlite/0005_members.sql"),
);

/// The Postgres form of [`MIGRATION_0005`], the same file byte for byte.
const MIGRATION_0005_POSTGRES: SqlMigration = SqlMigration::new(
    "0005",
    "members",
    include_str!("../migrations/postgres/0005_members.sql"),
);

/// Names and avatars (the profile change): `display_name` and `avatar` on a
/// member, `avatar` on a workspace.
const MIGRATION_0006: SqlMigration = SqlMigration::new(
    "0006",
    "profiles",
    include_str!("../migrations/sqlite/0006_profiles.sql"),
);

/// The Postgres form of [`MIGRATION_0006`], the same file byte for byte.
const MIGRATION_0006_POSTGRES: SqlMigration = SqlMigration::new(
    "0006",
    "profiles",
    include_str!("../migrations/postgres/0006_profiles.sql"),
);

/// Deleting a workspace (ADR 0004, "Deleting a workspace"): the workspace
/// keeps a tombstone (`deleted_at`) and its name and avatar are cleared.
const MIGRATION_0007: SqlMigration = SqlMigration::new(
    "0007",
    "delete_workspace",
    include_str!("../migrations/sqlite/0007_delete_workspace.sql"),
);

/// The Postgres form of [`MIGRATION_0007`], the same file byte for byte.
const MIGRATION_0007_POSTGRES: SqlMigration = SqlMigration::new(
    "0007",
    "delete_workspace",
    include_str!("../migrations/postgres/0007_delete_workspace.sql"),
);

/// What the composition does once a member has been removed (ADR 0004).
///
/// The tombstone this module writes already refuses the removed member's
/// sessions and personal access tokens, because both re-read the member row.
/// The seam is for the rows other modules own — the token rows themselves,
/// so a re-admitted person's old tokens stay dead; agent accounts the member
/// owned; grants naming them — which this module may not reach (the graph
/// runs `tokens` → `workspaces`).
#[async_trait::async_trait]
pub trait MemberRemoval: Send + Sync {
    /// `user_id` was just removed from `workspace_id`. Best effort: the
    /// removal has happened, and a failure here must not undo it.
    async fn removed(
        &self,
        db: &dyn Database,
        now: OffsetDateTime,
        workspace_id: &str,
        user_id: &str,
    );
}

/// What deleting a workspace does beyond this module's own rows (ADR 0004,
/// "Deleting a workspace").
///
/// The content of a workspace lives in page scopes this module cannot name (a
/// scope is a hash that the pages crate computes), and the tokens, agent
/// accounts, grants and provider keys belong to modules the graph runs
/// above this one. So the composition does those steps through this seam.
///
/// Every step must be idempotent: the deletion runs `erase` before it touches
/// this module's rows, so a failure here leaves the workspace in place and
/// the owner can run the deletion again. A failure aborts the deletion; it is
/// never best effort, because a deleted workspace whose content is still
/// readable is the one outcome the deletion exists to prevent. A composition
/// that cannot shred (no key custodian) must answer `Err`, not `Ok`.
#[async_trait::async_trait]
pub trait WorkspaceErasure: Send + Sync {
    /// Shreds the content of every space in `spaces` (`shared`,
    /// `personal:<member id>` or `channel:<platform>:<channel>`, as access
    /// grants name them) and revokes what other modules hold for
    /// `workspace_id`: personal access tokens, agent accounts and their keys,
    /// grants, and the workspace's provider and tool credentials.
    async fn erase(
        &self,
        db: &dyn Database,
        now: OffsetDateTime,
        workspace_id: &str,
        spaces: &[String],
    ) -> Result<(), ErasureFailed>;
}

/// The erasure could not complete; the deletion did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErasureFailed;

/// Records an email send into the cost ledger, for the composition to
/// inject (issue #12). A seam rather than a dependency because the graph
/// runs the other way: the usage module reaches this one through `tokens`
/// (`usage` → `tokens` → `workspaces`), so depending back on `usage` would
/// be a cycle Cargo refuses. The same shape as [`Answers`], whose page
/// store this module may not hold either.
#[async_trait::async_trait]
pub trait EmailLedger: Send + Sync {
    /// Record one send for `workspace_id`. The implementation swallows its
    /// own failures: a lost count must never fail the mail it counts.
    async fn record_email(&self, db: &dyn Database, now: OffsetDateTime, workspace_id: &str);
}

/// What a sign-in mail has to say, handed to a [`SignInMail`] renderer.
///
/// Only the three facts the mail is about, and nothing a renderer could leak
/// by accident: the token reaches it inside `link` because the link *is* the
/// mail, but neither the stored token hash nor the idempotency key does —
/// the key is set by this module after the renderer returns, so no renderer
/// can change what makes a provider retry collapse.
#[derive(Debug, Clone, Copy)]
pub struct SignInMailInput<'a> {
    /// The single-use link the mail exists to deliver.
    pub link: &'a str,
    /// How long the link lives, in whole minutes.
    pub minutes: i64,
    /// The address the mail goes to, for a "sent to … because …" line.
    pub recipient: &'a str,
    /// Set when the link is an invitation to join a workspace (ADR 0004)
    /// rather than a sign-in the recipient asked for.
    pub invitation: Option<Invitation<'a>>,
}

/// What an invitation mail says beyond the link.
#[derive(Debug, Clone, Copy)]
pub struct Invitation<'a> {
    /// The workspace's name.
    pub workspace: &'a str,
    /// The display name of the member who sent it.
    pub invited_by: &'a str,
}

impl SignInMailInput<'_> {
    /// How long the link lives, in words: minutes under two hours, hours
    /// under two days, days beyond.
    #[must_use]
    pub fn lifetime(&self) -> String {
        let minutes = self.minutes;
        if minutes < 120 {
            format!("{minutes} minutes")
        } else if minutes < 2 * 24 * 60 {
            format!("{} hours", minutes / 60)
        } else {
            format!("{} days", minutes / (24 * 60))
        }
    }
}

/// Renders the sign-in mail's subject, HTML and text, for the composition to
/// inject (the branded sign-in mail).
///
/// A seam rather than a dependency for the reason the crate's `Cargo.toml`
/// records: a module may name only the Cratefield ports, so the template
/// crate and the venture's theme — both of which belong to the composition —
/// cannot be reached from here. What this module may hold is "a way to turn
/// a link into a mail", which is exactly this. With none injected the module
/// renders its own plain mail, so a composition with no theme still signs
/// people in.
pub trait SignInMail: Send + Sync {
    /// The rendered mail for `input`. Infallible on purpose: a renderer has
    /// nothing to fail on but its own bug, and a sign-in that cannot send
    /// because a theme was malformed is worse than an unbranded one — a
    /// composition that can fail validates its theme before injecting it.
    fn render(&self, input: &SignInMailInput<'_>) -> cratefield_core::Rendered;
}

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
    /// The seam the cost ledger counts an email send through (issue #12).
    /// `None` is a composition that mounted no usage module: mail still
    /// sends, nothing counts it.
    ledger: Option<Arc<dyn EmailLedger>>,
    /// The seam the sign-in mail renders through. `None` is the module's
    /// own plain mail.
    sign_in_mail: Option<Arc<dyn SignInMail>>,
    /// The seam a member removal cleans up through (ADR 0004).
    removal: Option<Arc<dyn MemberRemoval>>,
    /// The seam a workspace deletion shreds and revokes through. Without it
    /// the deletion route refuses: nothing may be deleted that cannot be
    /// shredded.
    erasure: Option<Arc<dyn WorkspaceErasure>>,
}

// The seams are `dyn`s and not a `Debug`; the module itself is still
// one, so a composition that prints its modules keeps working.
impl std::fmt::Debug for Workspaces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspaces")
            .field("answers", &self.answers.is_some())
            .field("turns", &self.turns.is_some())
            .field("ledger", &self.ledger.is_some())
            .field("sign_in_mail", &self.sign_in_mail.is_some())
            .field("removal", &self.removal.is_some())
            .field("erasure", &self.erasure.is_some())
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

    /// Count each email send into `ledger` (issue #12).
    ///
    /// Injected rather than built here for the reason recorded on
    /// [`EmailLedger`]: this module cannot depend on the usage crate.
    #[must_use]
    pub fn email_ledger(mut self, ledger: Arc<dyn EmailLedger>) -> Self {
        self.ledger = Some(ledger);
        self
    }

    /// Render each sign-in mail through `mail`, the composition's branded
    /// renderer, instead of the module's plain one.
    ///
    /// Injected rather than built here for the reason recorded on
    /// [`SignInMail`]: the theme and the template crate are the venture's.
    /// The per-link idempotency key is still this module's, set after `mail`
    /// renders, so a renderer cannot weaken it.
    #[must_use]
    pub fn sign_in_mail(mut self, mail: Arc<dyn SignInMail>) -> Self {
        self.sign_in_mail = Some(mail);
        self
    }

    /// Clean up through `removal` once a member is removed (ADR 0004).
    #[must_use]
    pub fn on_member_removed(mut self, removal: Arc<dyn MemberRemoval>) -> Self {
        self.removal = Some(removal);
        self
    }

    /// Shred and revoke through `erasure` when an owner deletes a workspace.
    #[must_use]
    pub fn on_workspace_erasure(mut self, erasure: Arc<dyn WorkspaceErasure>) -> Self {
        self.erasure = Some(erasure);
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
    ///
    /// `Blob` is optional for the same reason again: it holds the workspace's
    /// avatar image, and a deployment with no blob store answers `503` on the
    /// avatar routes and keeps every name and every other route.
    fn optional(&self) -> &'static [Port] {
        &[Port::Mailer, Port::Classifier, Port::Blob]
    }

    /// The avatar upload is the one route here that takes more than the
    /// harness's 64 KiB: an image up to [`AVATAR_MAX_BYTES`], plus room for
    /// the refusal to be a precise `413` from the handler rather than the
    /// transport's. Every other route keeps the default through its own
    /// extractor limit.
    fn max_body_bytes(&self, _cfg: &dyn Config) -> usize {
        profile::AVATAR_BODY_CEILING
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
            "turn_policies",
            "turn_triage",
            "workspace_invites",
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
                description: "The workspace's id and name, the id of the person who \
                              first signed in — the workspace owner — and the id of the \
                              workspace's avatar image, if an owner or admin uploaded one.",
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
                              reports for you, the name you chose for yourself there, \
                              the id of the avatar image you uploaded there, your \
                              timezone and whether Slack marks you an admin. The \
                              avatar image itself is kept in the blob store under that \
                              id and is served only to members of the same workspace \
                              while this row names it.",
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
            // The two tables issue #9 added. Neither names a person: the
            // policy row is a workspace's (or channel's) choice of
            // proactivity, and the triage row is what one *message* decided
            // and cost — platform ids and numbers, never text or author.
            PersonalDataSet {
                table: "workspace_invites",
                subject: "email",
                kind: DataKind::Contact,
                disposition: Disposition::Erase,
                description: "An invitation to a workspace sent to your email address: \
                              the role offered, who sent it, when it lapses, and whether \
                              you accepted it.",
                redacted: &["link_hash", "invited_by"],
                subject_via: None,
            },
            PersonalDataSet::none(
                "turn_policies",
                "the row is one channel's (or the workspace's org-wide) \
                 speak-up setting: how eagerly the brain may answer a \
                 message that did not name it",
            ),
            PersonalDataSet::none(
                "turn_triage",
                "the row is one message's triage outcome: the channel and \
                 message ids, whether it addressed the bot, the proactivity \
                 in force, the decision and estimated token counts — never \
                 the message text or its author",
            ),
        ];
        SETS
    }

    fn migrations(&self) -> Migrations {
        const MIGRATIONS: [SqlMigration; 7] = [
            MIGRATION_INIT,
            MIGRATION_0002,
            MIGRATION_0003,
            MIGRATION_0004,
            MIGRATION_0005,
            MIGRATION_0006,
            MIGRATION_0007,
        ];
        // The array is the apply order; this refuses a gap, a duplicate or
        // an entry out of order at build time.
        const _: () = cratefield_core::assert_migration_set(&MIGRATIONS);
        const MIGRATIONS_POSTGRES: [SqlMigration; 7] = [
            MIGRATION_INIT_POSTGRES,
            MIGRATION_0002_POSTGRES,
            MIGRATION_0003_POSTGRES,
            MIGRATION_0004_POSTGRES,
            MIGRATION_0005_POSTGRES,
            MIGRATION_0006_POSTGRES,
            MIGRATION_0007_POSTGRES,
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
        handlers::router(
            ctx,
            self.answers.clone(),
            self.turns.clone(),
            self.ledger.clone(),
            self.sign_in_mail.clone(),
            self.removal.clone(),
            self.erasure.clone(),
        )
    }
}
