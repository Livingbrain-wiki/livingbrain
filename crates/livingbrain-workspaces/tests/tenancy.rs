//! Tenancy across both sign-in methods: a Slack-created workspace keeps the
//! id it had, a Slack team belongs to at most one workspace, and a member
//! of one workspace can never read another however they signed in.
//!
//! These are the isolation properties ADR 0002 rests on. Issue #71 replaced
//! "the workspace id *is* the Slack team id" with "the workspace id is ours
//! and Slack is a connection", so each of them needs guarding again — a
//! test that only ever signed in with Slack would pass either way.

mod support;

use cratefield_core::axum::http::StatusCode;
use cratefield_core::{Kid, Payload, Signer as _};
use cratefield_testing::TestHarness;
use livingbrain_workspaces::{
    FLOW_COOKIE, FLOW_PURPOSE, LinkOutcome, SESSION_COOKIE, link_connection, link_identity,
};
use serde_json::{Value, json};
use support::*;

const ME: &str = "/v1/workspaces/me";
const MEMBERS: &str = "/v1/workspaces/members";
const LINK_START: &str = "/v1/workspaces/connections/slack/start";
const EMAIL_START: &str = "/v1/workspaces/email/start";
const EMAIL_VERIFY: &str = "/v1/workspaces/email/verify";

const TEAM_A: &str = "T0TENANCYAA";
const TEAM_B: &str = "T0TENANCYBB";
const ALICE: &str = "U0ALICEAA";
const BOB: &str = "U0BOBBBBB";
const DAVE: &str = "U0DAVEDDD";
const ERIN_SLACK: &str = "U0ERINEEE";
const ERIN: &str = "erin@example.test";
const ALICE_EMAIL: &str = "alice@example.test";

// ---------------------------------------------------------------------------
// A Slack-created workspace keeps the id it had

#[pollster::test]
async fn a_slack_workspace_keeps_the_legacy_id_the_migration_left_it() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // A workspace as ADR 0001 wrote it: the id *is* the team id. The
    // fixture then runs migration `0002`'s own backfill statements, so the
    // Slack links are the migration's output and not this test's.
    seed_legacy_workspace(&kit, "T0LEGACY", "Legacy", "U0LEGACY1").await;
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = 'T0LEGACY'",
            vec![],
        )
        .await
        .as_deref(),
        Some("T0LEGACY"),
        "the backfill linked the legacy id as a Slack team"
    );

    // Somebody in that team signs in, exactly as they did before the
    // migration.
    let session = sign_in(&kit, &http, "T0LEGACY", ALICE, "Alice").await;
    assert_eq!(
        session.workspace_id, "T0LEGACY",
        "ids are not rewritten: the id is opaque now, not a team id"
    );

    let me = get(&kit.router, ME, &session.cookies()).await.json();
    assert_eq!(me["workspace"]["id"], "T0LEGACY");
    assert_eq!(me["workspace"]["name"], "Legacy");
    assert_eq!(
        me["workspace"]["owner_id"], "U0LEGACY1",
        "the first person through this workspace still owns it"
    );
    assert_eq!(me["is_owner"], false, "Alice is a later signer");
    assert_eq!(me["member"]["user_id"], ALICE);

    // She joins the workspace that already existed rather than minting a
    // second one, and the migration's own member row is untouched.
    assert_eq!(count(&kit, "SELECT id FROM workspaces").await, 1);
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &session.cookies()).await.json()),
        vec!["U0ALICEAA".to_owned(), "U0LEGACY1".to_owned()]
    );

    // Signing in again, and through the email link, both land in it too.
    link_identity(
        &*kit.db,
        "T0LEGACY",
        "email",
        "alice@example.test",
        ALICE,
        RFC3339_NOW,
    )
    .await
    .expect("the address is an identity of Alice's member row");
    post_json(
        &kit.router,
        EMAIL_START,
        &[],
        &json!({"email": "alice@example.test", "workspace_id": "T0LEGACY"}),
    )
    .await;
    let by_email = post_form(
        &kit.router,
        EMAIL_VERIFY,
        &[],
        &format!("token={}", mailed_token(&kit)),
    )
    .await
    .cookie(SESSION_COOKIE)
    .expect("a session");
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &by_email)])
        .await
        .json();
    assert_eq!(me["workspace"]["id"], "T0LEGACY");
    assert_eq!(count(&kit, "SELECT id FROM workspaces").await, 1);
}

// ---------------------------------------------------------------------------
// One Slack team, at most one workspace

#[pollster::test]
async fn the_database_refuses_a_second_workspace_for_one_slack_team() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    seed_workspace(&kit, "ws_FIRST", "First", ALICE).await;
    seed_workspace(&kit, "ws_SECOND", "Second", BOB).await;

    const INSERT: &str = "INSERT INTO workspace_connections \
         (workspace_id, platform, external_id, created_at) VALUES (?, ?, ?, ?)";
    assert!(
        exec_result(
            &kit,
            INSERT,
            vec![
                text("ws_FIRST"),
                text("slack"),
                text("T0ONCE"),
                text(RFC3339_NOW)
            ],
        )
        .await
        .is_ok(),
        "the first link is written"
    );

    // `UNIQUE (platform, external_id)`: one Slack team names one workspace,
    // and no route, seam or script can make it name two.
    let second = exec_result(
        &kit,
        INSERT,
        vec![
            text("ws_SECOND"),
            text("slack"),
            text("T0ONCE"),
            text(RFC3339_NOW),
        ],
    )
    .await;
    assert!(
        second.is_err(),
        "a second workspace for one team is refused"
    );
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = 'T0ONCE'",
            vec![],
        )
        .await
        .as_deref(),
        Some("ws_FIRST"),
        "and the row that holds the team is left as it was"
    );

    // The other half of the key: `PRIMARY KEY (workspace_id, platform)`, so
    // a workspace has at most one connection per platform and a second
    // team cannot join it by the back door.
    assert!(
        exec_result(
            &kit,
            INSERT,
            vec![
                text("ws_FIRST"),
                text("slack"),
                text("T0SECONDTEAM"),
                text(RFC3339_NOW),
            ],
        )
        .await
        .is_err(),
        "a second Slack team for one workspace is refused too"
    );

    // The same team on another platform is a different key, and is allowed
    // — a Discord guild id may legitimately look like a Slack team id.
    assert!(
        exec_result(
            &kit,
            INSERT,
            vec![
                text("ws_SECOND"),
                text("discord"),
                text("T0ONCE"),
                text(RFC3339_NOW),
            ],
        )
        .await
        .is_ok(),
        "the constraint is per platform"
    );
}

#[pollster::test]
async fn the_link_seam_reports_a_clash_and_never_rewrites_the_row() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    seed_workspace(&kit, "ws_FIRST", "First", ALICE).await;
    seed_workspace(&kit, "ws_SECOND", "Second", BOB).await;
    let db = &*kit.db;

    assert_eq!(
        link_connection(db, "ws_FIRST", "slack", "T0TEAM", RFC3339_NOW)
            .await
            .expect("the first link"),
        LinkOutcome::Linked
    );
    let written_at = created_at(&kit, "T0TEAM").await;
    assert_eq!(written_at.as_deref(), Some(RFC3339_NOW));

    // The same workspace again is the state the caller asked for, so it is
    // a success rather than an error.
    assert_eq!(
        link_connection(db, "ws_FIRST", "slack", "T0TEAM", RFC3339_NOW)
            .await
            .expect("re-linking"),
        LinkOutcome::AlreadyThisWorkspace
    );

    // Another workspace is a clash, and it is the caller's to resolve: a
    // deployment unlinks the other side deliberately, never by this seam
    // overwriting it.
    assert_eq!(
        link_connection(db, "ws_SECOND", "slack", "T0TEAM", RFC3339_NOW)
            .await
            .expect("the clash is reported, not raised"),
        LinkOutcome::Conflict
    );
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = 'T0TEAM'",
            vec![],
        )
        .await
        .as_deref(),
        Some("ws_FIRST"),
        "the row still says ws_FIRST"
    );
    assert_eq!(
        created_at(&kit, "T0TEAM").await,
        written_at,
        "and it was not rewritten"
    );

    // A team nobody has linked is free for anyone.
    assert_eq!(
        link_connection(db, "ws_SECOND", "slack", "T0FRESH", RFC3339_NOW)
            .await
            .expect("a free team"),
        LinkOutcome::Linked
    );
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = 'T0FRESH'",
            vec![],
        )
        .await
        .as_deref(),
        Some("ws_SECOND")
    );
}

#[pollster::test]
async fn only_the_owner_may_start_linking_a_slack_team() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let bob = sign_in(&kit, &http, TEAM_A, BOB, "Bob").await;

    // No session at all is a 401, and never reaches Slack.
    let before = http.requests().len();
    assert_eq!(
        get(&kit.router, LINK_START, &[]).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(http.requests().len(), before, "Slack was never called");

    // The owner may start, and the workspace and member ride in the sealed
    // flow cookie rather than in the authorize URL — a query parameter is
    // a value the caller could have edited, and this one decides which
    // workspace a team is bound to.
    let start = get(&kit.router, LINK_START, &alice.cookies()).await;
    assert_eq!(start.status, StatusCode::FOUND);
    assert!(
        start
            .location()
            .starts_with("https://slack.com/openid/connect/authorize?"),
        "{}",
        start.location()
    );
    assert!(!start.location().contains(&alice.workspace_id));
    let payload = flow_payload(&kit, &start.cookie(FLOW_COOKIE).expect("a flow cookie"));
    assert_eq!(payload["workspace_id"], alice.workspace_id);
    assert_eq!(payload["user_id"], ALICE);

    // A member who is not the owner is refused here, at the route that
    // starts the flow, and is not even sent to Slack: a member cannot
    // attach a team to a workspace they do not own, and there is nothing
    // they could do with the round trip. The 403 is the same one the
    // callback answers, so a client sees one rule and one answer.
    let refused = get(&kit.router, LINK_START, &bob.cookies()).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(
        refused.location().is_empty(),
        "not a redirect to Slack: {}",
        refused.location()
    );
    assert!(
        refused.cookie(FLOW_COOKIE).is_none(),
        "and no flow cookie to complete"
    );
    assert_eq!(
        http.requests().len(),
        before,
        "the refusal never reached Slack"
    );
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections              WHERE platform = 'slack' AND external_id = ?",
            vec![text(TEAM_B)],
        )
        .await,
        None,
        "and no team was attached to anybody"
    );

    // Bob's own session is untouched by the refusal, and he is still a
    // member of Alice's workspace and still not its owner.
    let me = get(&kit.router, ME, &bob.cookies()).await.json();
    assert_eq!(me["workspace"]["id"], alice.workspace_id);
    assert_eq!(me["is_owner"], false);
}

#[pollster::test]
async fn a_flow_cookie_minted_before_the_owner_check_still_cannot_link() {
    let http = TokenHttp::new();
    let kit = harness(&http);
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    // Bob is a member of Alice's workspace, and not its owner.
    sign_in(&kit, &http, TEAM_A, BOB, "Bob").await;
    // A flow cookie naming Bob and his workspace is what the start route
    // used to hand any member. It is sealed here the way the module seals
    // one, because a cookie minted before the owner check reached the
    // start route is still a valid one for the rest of its ten minutes.
    let state = "0STATE00000000000000000000";
    let nonce = "0NONCE00000000000000000000";
    let stale = kit.signer.sign(&Payload {
        purpose: FLOW_PURPOSE.to_owned(),
        subject: json!({
            "state": state,
            "nonce": nonce,
            "expires_at": NOW + 600,
            "workspace_id": alice.workspace_id,
            "user_id": BOB,
        })
        .to_string(),
        // Expiry rides inside the payload so it is checked against the
        // Clock port, exactly as the module seals it.
        exp: None,
        kid: Kid::Cur,
    });

    // Slack answers with a team Bob really is in, and the callback still
    // refuses: the owner rule is enforced where the link is written, not
    // only where it starts.
    http.will_return(&id_token(&claims(nonce, TEAM_B, BOB, "Bob")));
    let refused = get(
        &kit.router,
        &format!("/v1/workspaces/slack/callback?code=c&state={state}"),
        &[(FLOW_COOKIE, &stale)],
    )
    .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(
        refused.cookie(SESSION_COOKIE).is_none(),
        "a refused link seals no session"
    );
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = ?",
            vec![text(TEAM_B)],
        )
        .await,
        None,
        "and no team was attached to anybody"
    );
}

#[pollster::test]
async fn a_team_linked_to_another_workspace_answers_409() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // Alice owns the workspace team A is linked to; Dave owns another.
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    let dave = sign_in(&kit, &http, TEAM_B, DAVE, "Dave").await;
    assert_ne!(alice.workspace_id, dave.workspace_id);

    // Dave, who owns his own workspace, tries to claim Alice's team.
    let start = get(&kit.router, LINK_START, &dave.cookies()).await;
    assert_eq!(start.status, StatusCode::FOUND);
    let flow = start.cookie(FLOW_COOKIE).expect("a flow cookie");
    let payload = flow_payload(&kit, &flow);
    let nonce = payload["nonce"].as_str().expect("a nonce").to_owned();
    let state = payload["state"].as_str().expect("a state").to_owned();

    http.will_return(&id_token(&claims(&nonce, TEAM_A, DAVE, "Dave")));
    let callback = get(
        &kit.router,
        &format!("/v1/workspaces/slack/callback?code=c&state={state}"),
        &[(FLOW_COOKIE, &flow)],
    )
    .await;
    assert_eq!(callback.status, StatusCode::CONFLICT);
    assert!(
        callback.cookie(SESSION_COOKIE).is_none(),
        "a refused link seals no session"
    );

    // Nothing moved: the team is still Alice's workspace's, and Dave's has
    // no connection at all.
    assert_eq!(
        column(
            &kit,
            "SELECT workspace_id AS value FROM workspace_connections \
             WHERE platform = 'slack' AND external_id = ?",
            vec![text(TEAM_A)],
        )
        .await
        .as_deref(),
        Some(alice.workspace_id.as_str())
    );
    assert_eq!(
        linked_team(&kit, &dave.workspace_id).await.as_deref(),
        Some(TEAM_B),
        "Dave's workspace still has only the team it was created from"
    );
    // And Dave's own session still works and is still not an owner of A.
    let me = get(&kit.router, ME, &dave.cookies()).await.json();
    assert_eq!(me["workspace"]["id"], dave.workspace_id);
    assert_eq!(me["is_owner"], true);
}

// ---------------------------------------------------------------------------
// One person, one member row — whichever door they came through

/// A person whose workspace came from their email address, and who then
/// links Slack and signs in with it, is the *same member*. Before the
/// identity was resolved in `sign_in_slack_workspace`, the Slack sign-in
/// took the Slack user id as the member id unconditionally: a second member
/// row appeared, `/members` listed her twice, `/me` answered a different
/// `user_id` depending on which door she used, and because `owner_id` was
/// the `usr_` id the email sign-in issued, signing in with Slack reported
/// `is_owner: false` for the person who owned the workspace.
#[pollster::test]
async fn a_person_who_links_slack_to_an_email_workspace_is_still_one_member() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // Alice creates a workspace with her address alone: a `ws_` id, a
    // `usr_` member, and no Slack in it anywhere.
    post_json(
        &kit.router,
        EMAIL_START,
        &[],
        &json!({"email": ALICE_EMAIL, "workspace_name": "Alice"}),
    )
    .await;
    let by_email = post_form(
        &kit.router,
        EMAIL_VERIFY,
        &[],
        &format!("token={}", mailed_token(&kit)),
    )
    .await
    .cookie(SESSION_COOKIE)
    .expect("a session");
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &by_email)])
        .await
        .json();
    let workspace_id = me["workspace"]["id"].as_str().expect("an id").to_owned();
    let user_id = me["member"]["user_id"]
        .as_str()
        .expect("a member id")
        .to_owned();
    assert!(user_id.starts_with("usr_"), "an email-first id: {user_id}");

    // She links her Slack account to her own workspace, which binds the
    // Slack user id to the member row she already has.
    link_team(
        &kit,
        &http,
        &[(SESSION_COOKIE, &by_email)],
        TEAM_A,
        ALICE,
        "Alice",
    )
    .await;
    assert_eq!(
        column(
            &kit,
            "SELECT user_id AS value FROM member_identities \
             WHERE workspace_id = ? AND platform = 'slack' AND external_id = ?",
            vec![text(&workspace_id), text(ALICE)],
        )
        .await
        .as_deref(),
        Some(user_id.as_str()),
        "the linking route bound the Slack id to the member she already was"
    );

    // Now she signs in with Slack, into the team her own workspace is
    // linked to.
    let by_slack = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    assert_eq!(
        by_slack.workspace_id, workspace_id,
        "the same workspace, resolved through the link she made"
    );

    // One member, not two.
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &by_slack.cookies()).await.json()),
        vec![user_id.clone()],
        "the Slack sign-in did not mint a second member row"
    );
    assert_eq!(
        count(&kit, "SELECT user_id FROM workspace_members").await,
        1
    );

    // The same member id whichever way she is asked, and the ownership her
    // email sign-in reported still holds through the Slack one.
    let slack_me = get(&kit.router, ME, &by_slack.cookies()).await.json();
    assert_eq!(slack_me["member"]["user_id"], user_id);
    assert_eq!(
        email_me(&kit, &by_email).await["member"]["user_id"],
        slack_me["member"]["user_id"],
        "`/me` answers the same person either way"
    );
    assert_eq!(slack_me["is_owner"], true, "she owns it she made");
    assert_eq!(slack_me["workspace"]["owner_id"], user_id);
    assert_eq!(
        email_me(&kit, &by_email).await["is_owner"],
        true,
        "and the email door still says so"
    );
}

/// The other side of the same resolution: a person the workspace has never
/// seen keeps the Slack user id as their member id. That is the Slack-first
/// path this module started as, and `0001`'s rows, migration `0002`'s
/// backfill and issue #6's `apply_user_change` seam all name Slack user ids
/// as member ids — so this must not change.
#[pollster::test]
async fn a_slack_first_member_still_keeps_the_slack_user_id() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // A team this deployment has never seen: nobody has linked it, so
    // there is no identity to resolve and the Slack user id is the member.
    let fresh = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &fresh.cookies()).await.json()),
        vec![ALICE.to_owned()]
    );
    let me = get(&kit.router, ME, &fresh.cookies()).await.json();
    assert_eq!(me["member"]["user_id"], ALICE);
    assert_eq!(me["is_owner"], true);
    assert_eq!(
        session_payload(&kit, &fresh.cookie)["user_id"],
        ALICE,
        "and the session names it too"
    );

    // A workspace as migration `0002`'s backfill left it: the identity
    // already says the Slack user id *is* the member id, so resolving it
    // changes nothing and adds nothing.
    seed_legacy_workspace(&kit, "T0LEGACYB", "Legacy", BOB).await;
    let legacy = sign_in(&kit, &http, "T0LEGACYB", BOB, "Bob").await;
    assert_eq!(legacy.workspace_id, "T0LEGACYB");
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &legacy.cookies()).await.json()),
        vec![BOB.to_owned()],
        "resolving an identity that already names the Slack id adds no row"
    );
    assert_eq!(
        get(&kit.router, ME, &legacy.cookies()).await.json()["member"]["user_id"],
        BOB
    );
}

// ---------------------------------------------------------------------------
// Isolation, whichever way each person signed in

#[pollster::test]
async fn a_slack_member_cannot_read_an_email_workspace() {
    cross_reads(true).await;
}

#[pollster::test]
async fn an_email_member_cannot_read_a_slack_workspace() {
    cross_reads(false).await;
}

/// Alice signs in with Slack; Erin creates a workspace with her address
/// alone. Each sees only their own, in whichever order the two were set
/// up — the direction of the pair is not what makes it hold.
async fn cross_reads(slack_first: bool) {
    let http = TokenHttp::new();
    let kit = harness(&http);

    let (alice, erin) = if slack_first {
        (
            sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await,
            email_workspace(&kit).await,
        )
    } else {
        let erin = email_workspace(&kit).await;
        (sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await, erin)
    };
    assert_ne!(alice.workspace_id, erin.workspace_id);

    // Each session reads its own workspace and its own members only.
    let a_members = user_ids(&get(&kit.router, MEMBERS, &alice.cookies()).await.json());
    assert_eq!(a_members, vec![ALICE.to_owned()]);
    let e_members = user_ids(&get(&kit.router, MEMBERS, &erin.cookies()).await.json());
    assert_eq!(e_members, vec![erin.user_id.clone()]);

    let a_me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(a_me["workspace"]["id"], alice.workspace_id);
    assert_eq!(a_me["is_owner"], true);
    let e_me = get(&kit.router, ME, &erin.cookies()).await.json();
    assert_eq!(e_me["workspace"]["id"], erin.workspace_id);
    assert_eq!(e_me["is_owner"], true);
    assert_eq!(e_me["workspace"]["owner_id"], erin.user_id);

    // A session naming a workspace it is not a member of is refused
    // exactly as no session at all is — whichever way it was sealed, and
    // whichever workspace id the claim carries.
    for (workspace_id, user_id, who) in [
        (
            &alice.workspace_id,
            erin.user_id.as_str(),
            "Erin in Alice's workspace",
        ),
        (&erin.workspace_id, ALICE, "Alice in Erin's workspace"),
        (
            &alice.workspace_id,
            "U0NOBODY0",
            "a stranger in Alice's workspace",
        ),
        (
            &erin.workspace_id,
            "U0NOBODY0",
            "a stranger in Erin's workspace",
        ),
    ] {
        let crossed = signed_session(&kit, workspace_id, user_id, NOW + 3600);
        for path in [ME, MEMBERS] {
            assert_eq!(
                get(&kit.router, path, &[(SESSION_COOKIE, &crossed)])
                    .await
                    .status,
                StatusCode::UNAUTHORIZED,
                "{who}, {path}"
            );
        }
    }

    // The member rows are per workspace even where the ids are equal: two
    // workspaces, one Slack user id, is not one membership.
    let dave = sign_in(&kit, &http, TEAM_B, ALICE, "Alice").await;
    assert_ne!(dave.workspace_id, alice.workspace_id);
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &dave.cookies()).await.json()),
        vec![ALICE.to_owned()]
    );
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &alice.cookies()).await.json()),
        vec![ALICE.to_owned()],
        "the second workspace's member row did not join the first"
    );
}

#[pollster::test]
async fn a_member_who_joins_later_does_not_take_ownership() {
    let http = TokenHttp::new();
    let kit = harness(&http);

    // Alice signs in with Slack, so the owner is decided once, here.
    let alice = sign_in(&kit, &http, TEAM_A, ALICE, "Alice").await;
    assert_eq!(
        get(&kit.router, ME, &alice.cookies()).await.json()["is_owner"],
        true
    );

    // Bob signs in with Slack into the same team, and Erin with her address
    // alone into a workspace of her own — neither of which touches A.
    sign_in(&kit, &http, TEAM_A, BOB, "Bob").await;
    let hers = email_workspace(&kit).await;

    // Erin is in A by her Slack user id, and her address is bound to that
    // same member: a person's platform ids resolve to one member inside a
    // workspace, which is what lets a second sign-in method find them.
    sign_in(&kit, &http, TEAM_A, ERIN_SLACK, "Erin").await;
    link_identity(
        &*kit.db,
        &alice.workspace_id,
        "email",
        ERIN,
        ERIN_SLACK,
        RFC3339_NOW,
    )
    .await
    .expect("Erin's address is an identity of her member row in A");

    // She may now reach A by her address, and is a member of it who owns
    // nothing. The owner rule is set once and never rewritten, whichever
    // way a person arrives.
    let session = scoped_email_session(&kit, ERIN, &alice.workspace_id).await;
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    assert_eq!(me["workspace"]["id"], alice.workspace_id);
    assert_eq!(me["is_owner"], false);
    assert_eq!(me["workspace"]["owner_id"], ALICE);
    assert_eq!(
        me["member"]["user_id"], ERIN_SLACK,
        "her Slack id and her address are the same member here"
    );

    // The workspace her address created is still hers alone, and its
    // `usr_` member id is a different member of a different tenant.
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &hers.cookies()).await.json()),
        vec![hers.user_id.clone()]
    );

    // And Alice's own view never stops saying she owns it.
    let alice_me = get(&kit.router, ME, &alice.cookies()).await.json();
    assert_eq!(alice_me["is_owner"], true);
    assert_eq!(alice_me["workspace"]["owner_id"], ALICE);
    assert_eq!(
        user_ids(&get(&kit.router, MEMBERS, &alice.cookies()).await.json()),
        vec![ALICE.to_owned(), BOB.to_owned(), ERIN_SLACK.to_owned()]
    );
}

// ---------------------------------------------------------------------------
// Helpers

/// A workspace created from an email address alone, and the member id the
/// module gave the person.
struct EmailWorkspace {
    session: String,
    workspace_id: String,
    user_id: String,
}

impl EmailWorkspace {
    fn cookies(&self) -> [(&str, &str); 1] {
        [(SESSION_COOKIE, self.session.as_str())]
    }
}

async fn email_workspace(kit: &TestHarness) -> EmailWorkspace {
    post_json(&kit.router, EMAIL_START, &[], &json!({"email": ERIN})).await;
    let session = post_form(
        &kit.router,
        EMAIL_VERIFY,
        &[],
        &format!("token={}", mailed_token(kit)),
    )
    .await
    .cookie(SESSION_COOKIE)
    .expect("a session");
    let me = get(&kit.router, ME, &[(SESSION_COOKIE, &session)])
        .await
        .json();
    EmailWorkspace {
        session,
        workspace_id: me["workspace"]["id"].as_str().expect("an id").to_owned(),
        user_id: me["member"]["user_id"]
            .as_str()
            .expect("a member id")
            .to_owned(),
    }
}

/// A link scoped to a workspace the address is already an identity of.
async fn scoped_email_session(kit: &TestHarness, email: &str, workspace_id: &str) -> String {
    let start = post_json(
        &kit.router,
        EMAIL_START,
        &[],
        &json!({"email": email, "workspace_id": workspace_id}),
    )
    .await;
    assert_eq!(start.status, StatusCode::ACCEPTED);
    post_form(
        &kit.router,
        EMAIL_VERIFY,
        &[],
        &format!("token={}", mailed_token(kit)),
    )
    .await
    .cookie(SESSION_COOKIE)
    .expect("a session")
}

/// Runs the whole link round trip — the start route, then the callback
/// with a token naming `team_id` — for a caller already signed in. The
/// start route's owner rule and the callback's are both exercised, which is
/// the point: the cookie could in principle have been sealed without them.
async fn link_team(
    kit: &TestHarness,
    http: &TokenHttp,
    session: &[(&str, &str)],
    team_id: &str,
    user_id: &str,
    name: &str,
) {
    let start = get(&kit.router, LINK_START, session).await;
    assert_eq!(
        start.status,
        StatusCode::FOUND,
        "the link may be started: {}",
        String::from_utf8_lossy(&start.body)
    );
    let flow = start.cookie(FLOW_COOKIE).expect("a flow cookie");
    let payload = flow_payload(kit, &flow);
    let nonce = payload["nonce"].as_str().expect("a nonce").to_owned();
    let state = payload["state"].as_str().expect("a state").to_owned();

    http.will_return(&id_token(&claims(&nonce, team_id, user_id, name)));
    let callback = get(
        &kit.router,
        &format!("/v1/workspaces/slack/callback?code=c&state={state}"),
        &[(FLOW_COOKIE, &flow)],
    )
    .await;
    assert_eq!(
        callback.status,
        StatusCode::FOUND,
        "the link completes: {}",
        String::from_utf8_lossy(&callback.body)
    );
}

/// What `/me` answers for a session cookie, as JSON.
async fn email_me(kit: &TestHarness, session: &str) -> Value {
    get(&kit.router, ME, &[(SESSION_COOKIE, session)])
        .await
        .json()
}

/// The Slack team one workspace is linked to, or `None` when it has none.
async fn linked_team(kit: &TestHarness, workspace_id: &str) -> Option<String> {
    column(
        kit,
        "SELECT external_id AS value FROM workspace_connections          WHERE workspace_id = ? AND platform = 'slack'",
        vec![text(workspace_id)],
    )
    .await
}

/// When the row linking a Slack team was written, so a test can prove a
/// clashing attempt did not rewrite it.
async fn created_at(kit: &TestHarness, team: &str) -> Option<String> {
    rows(
        kit,
        "SELECT created_at FROM workspace_connections WHERE platform = 'slack' AND external_id = ?",
        vec![text(team)],
    )
    .await
    .first()
    .and_then(|row| row.get::<String>("created_at"))
}

async fn count(kit: &TestHarness, sql: &str) -> u64 {
    u64::try_from(rows(kit, sql, vec![]).await.len()).expect("a count")
}

fn user_ids(rows: &Value) -> Vec<String> {
    rows.as_array()
        .expect("an array")
        .iter()
        .filter_map(|row| row["user_id"].as_str().map(str::to_owned))
        .collect()
}
