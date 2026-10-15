//! The gate (acceptances A and B), on the wire: a write never runs without
//! the asker's own connection or an approved borrow, only the owner of a
//! pending ask can decide it in its own workspace, and a disabled tool is
//! neither offered nor run. The fake server records everything it is sent,
//! so every "nothing left the building" claim is a fact about the wire.

mod support;

use livingbrain_access::{Approval, UserId};
use livingbrain_channel::Platform;
use livingbrain_tools::{ToolOutcome, ToolRequest, offered_for, resolve, run_tool};
use serde_json::{Value, json};

use support::{OTHER, OWNER, PROVIDER, TOKEN, WORKSPACE, member, message, setup, slack};

/// One ask through the gate: `asker` on `owner`'s connection.
fn ask<'a>(
    asker: &'a UserId,
    owner: &'a UserId,
    tool: &'a str,
    arguments: &'a Value,
) -> ToolRequest<'a> {
    ToolRequest {
        workspace: WORKSPACE,
        asker,
        owner,
        provider: PROVIDER,
        tool,
        arguments,
    }
}

/// Files a borrowed write — `OTHER` asking to run `create_issue` on
/// `OWNER`'s connection — and returns its approval id.
fn pended(env: &support::Setup) -> String {
    let asker = member(OTHER);
    let owner = member(OWNER);
    let arguments = json!({"title": "Fix the gate"});
    let ToolOutcome::Pending { approval_id, .. } = pollster::block_on(run_tool(
        &env.gate(),
        &ask(&asker, &owner, "create_issue", &arguments),
        &slack(),
        &message(),
    )) else {
        panic!("a borrowed write pends");
    };
    approval_id
}

/// A write the owner did not ask for: filed, carded, and nothing sent.
#[test]
fn a_teammates_write_pends_and_no_call_leaves() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let asker = member(OTHER);
    let owner = member(OWNER);
    let arguments = json!({"title": "Fix the gate"});
    let outcome = pollster::block_on(run_tool(
        &env.gate(),
        &ask(&asker, &owner, "create_issue", &arguments),
        &slack(),
        &message(),
    ));
    let ToolOutcome::Pending { approval_id, card } = outcome else {
        panic!("a borrowed write pends, got {outcome:?}");
    };
    // The card is the owner's decision, named after the ask.
    assert_eq!(card.platform, Platform::Slack);
    let actions = &card.body["blocks"][1]["elements"];
    assert_eq!(actions[0]["action_id"], format!("approve:{approval_id}"));
    assert_eq!(actions[1]["action_id"], format!("deny:{approval_id}"));
    // The ask is on file whole, so resolving it replays exactly this call.
    assert_eq!(
        pollster::block_on(env.pending_row(&approval_id)),
        Some(json!({
            "asker": OTHER,
            "tool": "create_issue",
            "arguments": arguments.to_string(),
        }))
    );
    // And no tools/call left the building.
    assert!(env.http.tool_calls().is_empty(), "{:?}", env.http.sent());
}

/// Deny: the row is spent, nothing was sent, and a second resolve of the
/// same id finds nothing left to resolve.
#[test]
fn a_denial_spends_the_ask_and_sends_nothing() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let approval_id = pended(&env);
    let owner = member(OWNER);
    let outcome = pollster::block_on(resolve(
        &env.gate(),
        WORKSPACE,
        &owner,
        &approval_id,
        Approval::Deny,
    ));
    assert!(
        matches!(&outcome, ToolOutcome::Refused(reason) if reason.contains(OWNER)),
        "a denial refuses, got {outcome:?}"
    );
    assert!(env.http.tool_calls().is_empty(), "{:?}", env.http.sent());
    assert_eq!(pollster::block_on(env.pending_row(&approval_id)), None);
    // An approval is single-use: the id is spent, whoever resolves it.
    assert!(matches!(
        pollster::block_on(resolve(
            &env.gate(),
            WORKSPACE,
            &owner,
            &approval_id,
            Approval::Deny
        )),
        ToolOutcome::NotFound
    ));
    assert!(env.http.tool_calls().is_empty());
}

/// Approve: exactly one call, replaying the recorded ask, signed with the
/// owner's opened token — and then the id is spent.
#[test]
fn an_approval_runs_the_recorded_call_once() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let approval_id = pended(&env);
    let owner = member(OWNER);
    let outcome = pollster::block_on(resolve(
        &env.gate(),
        WORKSPACE,
        &owner,
        &approval_id,
        Approval::Approve,
    ));
    let ToolOutcome::Ran(result) = outcome else {
        panic!("an approval runs, got {outcome:?}");
    };
    assert!(!result.is_error);
    assert_eq!(result.content[0]["text"], "done");
    let calls = env.http.tool_calls();
    assert_eq!(calls.len(), 1, "exactly one call: {:?}", env.http.sent());
    // The token is the owner's, opened on the call path — the envelope's
    // only exit.
    assert_eq!(
        calls[0].authorization.as_deref(),
        Some(&format!("Bearer {TOKEN}")[..])
    );
    assert!(
        calls[0].body.contains("\"create_issue\""),
        "{}",
        calls[0].body
    );
    assert!(calls[0].body.contains("Fix the gate"), "{}", calls[0].body);
    assert_eq!(pollster::block_on(env.pending_row(&approval_id)), None);
    assert!(matches!(
        pollster::block_on(resolve(
            &env.gate(),
            WORKSPACE,
            &owner,
            &approval_id,
            Approval::Approve
        )),
        ToolOutcome::NotFound
    ));
    assert_eq!(env.http.tool_calls().len(), 1, "no extra call");
}

/// Only the owner of the connection, deciding in the workspace the ask was
/// filed in, can resolve it. Anyone else's click — a teammate holding the
/// id, or the owner from another workspace's resolve — consumes nothing:
/// the ask survives for the owner, and nothing is ever sent.
#[test]
fn only_the_owner_in_its_own_workspace_can_decide() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let approval_id = pended(&env);
    let owner = member(OWNER);
    let other = member(OTHER);

    let outcome = pollster::block_on(resolve(
        &env.gate(),
        WORKSPACE,
        &other,
        &approval_id,
        Approval::Approve,
    ));
    assert!(
        matches!(outcome, ToolOutcome::NotFound),
        "a non-owner is refused, got {outcome:?}"
    );
    assert!(env.http.tool_calls().is_empty());
    assert!(
        pollster::block_on(env.pending_row(&approval_id)).is_some(),
        "the ask survives a stranger's click"
    );

    // The same id resolved under another workspace's name is not this ask.
    let outcome = pollster::block_on(resolve(
        &env.gate(),
        "T0ELSEWHERE",
        &owner,
        &approval_id,
        Approval::Approve,
    ));
    assert!(matches!(outcome, ToolOutcome::NotFound));
    assert!(env.http.tool_calls().is_empty());
    assert!(pollster::block_on(env.pending_row(&approval_id)).is_some());

    // The owner, in the ask's own workspace, approves and it runs once.
    let outcome = pollster::block_on(resolve(
        &env.gate(),
        WORKSPACE,
        &owner,
        &approval_id,
        Approval::Approve,
    ));
    assert!(matches!(outcome, ToolOutcome::Ran(_)), "the owner approves");
    assert_eq!(env.http.tool_calls().len(), 1);
    assert!(pollster::block_on(env.pending_row(&approval_id)).is_none());
}

/// The asker's own connection needs nobody's approval, and an unknown id
/// resolves to nothing.
#[test]
fn the_owner_runs_a_write_immediately_and_an_unknown_id_resolves_nothing() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let owner = member(OWNER);
    let arguments = json!({});
    assert!(matches!(
        pollster::block_on(run_tool(
            &env.gate(),
            &ask(&owner, &owner, "create_issue", &arguments),
            &slack(),
            &message()
        )),
        ToolOutcome::Ran(_)
    ));
    assert_eq!(env.http.tool_calls().len(), 1);
    assert_eq!(
        env.http.tool_calls()[0].authorization.as_deref(),
        Some(&format!("Bearer {TOKEN}")[..])
    );
    assert!(matches!(
        pollster::block_on(resolve(
            &env.gate(),
            WORKSPACE,
            &owner,
            "01HZZZNOTAREALAPPROVAL",
            Approval::Approve
        )),
        ToolOutcome::NotFound
    ));
    assert_eq!(env.http.tool_calls().len(), 1);
}

/// A read-only tool borrows the teammate's connection; the same ask without
/// the annotation is a write and pends. The classification fails closed.
#[test]
fn a_read_borrows_and_an_unannotated_tool_does_not() {
    let env = setup();
    pollster::block_on(env.connect(OWNER, &[]));
    let asker = member(OTHER);
    let owner = member(OWNER);
    let arguments = json!({"query": "gate"});
    assert!(matches!(
        pollster::block_on(run_tool(
            &env.gate(),
            &ask(&asker, &owner, "search", &arguments),
            &slack(),
            &message()
        )),
        ToolOutcome::Ran(_)
    ));
    // The same borrow of a tool the server does not claim to be read-only
    // is a write: it pends.
    env.http.listing(json!([
        {"name": "search", "inputSchema": {"type": "object"},
         "annotations": {"readOnlyHint": true}},
        {"name": "export_all", "inputSchema": {"type": "object"}},
    ]));
    assert!(matches!(
        pollster::block_on(run_tool(
            &env.gate(),
            &ask(&asker, &owner, "export_all", &arguments),
            &slack(),
            &message()
        )),
        ToolOutcome::Pending { .. }
    ));
    // Reads are the only calls that ran.
    assert_eq!(env.http.tool_calls().len(), 1);
}

/// The disabled lists stop a call before even a `tools/list` reaches the
/// server — connection level, workspace level, whatever the case.
#[test]
fn a_disabled_tool_is_refused_before_anything_is_sent() {
    let env = setup();
    // The connection's switch spells the name its own way; case does not
    // smuggle the tool past it.
    pollster::block_on(env.connect(OWNER, &["Create_Issue"]));
    let owner = member(OWNER);
    let arguments = json!({});
    let outcome = pollster::block_on(run_tool(
        &env.gate(),
        &ask(&owner, &owner, "create_issue", &arguments),
        &slack(),
        &message(),
    ));
    assert!(matches!(
        outcome,
        ToolOutcome::Refused(reason) if reason.contains("disabled")
    ));
    assert!(env.http.sent().is_empty(), "{:?}", env.http.sent());

    // The workspace switch lands on tools the connection itself allows.
    pollster::block_on(env.set_workspace_disabled(&["search"]));
    let asker = member(OTHER);
    let outcome = pollster::block_on(run_tool(
        &env.gate(),
        &ask(&asker, &owner, "search", &arguments),
        &slack(),
        &message(),
    ));
    assert!(matches!(
        outcome,
        ToolOutcome::Refused(reason) if reason.contains("workspace")
    ));
    assert!(
        env.http.sent().is_empty(),
        "still nothing: {:?}",
        env.http.sent()
    );
}

/// A server that answers in `text/event-stream` bodies is read the same
/// way: the message for the request's id comes out of the stream.
#[test]
fn an_event_stream_answer_runs_like_a_json_one() {
    let env = setup();
    env.http.streaming();
    pollster::block_on(env.connect(OWNER, &[]));
    let owner = member(OWNER);
    let arguments = json!({});
    let outcome = pollster::block_on(run_tool(
        &env.gate(),
        &ask(&owner, &owner, "create_issue", &arguments),
        &slack(),
        &message(),
    ));
    let ToolOutcome::Ran(result) = outcome else {
        panic!("a streamed answer runs, got {outcome:?}");
    };
    assert_eq!(result.content[0]["text"], "done");
}

/// What the model is offered (acceptance B): a tool disabled on the
/// connection or on the workspace is absent from `offered_for` while the
/// fake server still lists it.
#[test]
fn disabled_tools_are_not_offered() {
    let env = setup();
    env.http.listing(json!([
        {"name": "search", "description": "Search", "inputSchema": {"type": "object"},
         "annotations": {"readOnlyHint": true}},
        {"name": "create_issue", "description": "Create an issue",
         "inputSchema": {"type": "object"}},
        {"name": "list_pages", "description": "List pages", "inputSchema": {"type": "object"},
         "annotations": {"readOnlyHint": true}},
    ]));
    pollster::block_on(env.connect(OWNER, &["search"]));
    pollster::block_on(env.set_workspace_disabled(&["create_issue"]));
    let owner = member(OWNER);
    let offered = pollster::block_on(offered_for(&env.gate(), WORKSPACE, &owner, PROVIDER))
        .expect("the offer reads");
    // The server lists all three; the model sees the one neither list named.
    assert_eq!(
        offered
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        ["list_pages"]
    );
}

/// A member who never connected the provider offers nothing — not an
/// error, just an empty tool list, and no request to any server.
#[test]
fn a_workspace_without_a_connection_offers_nothing() {
    let env = setup();
    let other = member(OTHER);
    let offered = pollster::block_on(offered_for(&env.gate(), WORKSPACE, &other, PROVIDER))
        .expect("no connection is not an error");
    assert!(offered.is_empty());
    assert!(env.http.sent().is_empty());
}
