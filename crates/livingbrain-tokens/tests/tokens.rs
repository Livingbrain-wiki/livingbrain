//! Personal access tokens over the wire (issue #72).
//!
//! Each acceptance criterion of the issue is held to here: a token is
//! returned exactly once and works on every later call, a token cannot mint
//! another token, a revoked token is refused by the same 401 an unknown one
//! is, a scope subset never reads outside itself, the token value never
//! appears in a response, a row or a log line after creation, and the device
//! flow mints a token usable with no browser at all.

mod support;

use cratefield_core::axum::http::StatusCode;
use livingbrain_access::{ChannelMemberships, Location, UserId, scopes_for};
use livingbrain_tokens::Member;
use serde_json::{Value, json};
use support::{API_BASE, Caller, Kit, OWNER, device_flow, prefix_of, rows};

/// A token is returned exactly once, by the creation, and every later call
/// authenticates with it.
#[pollster::test]
async fn a_token_is_returned_once_and_authenticates_every_later_call() {
    let kit = support::signed_in().await;

    let created = support::post_json(
        &kit,
        "/v1/tokens",
        &json!({"name": "Alice's laptop"}),
        &kit.owner(),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let body = created.json();
    let token = body["token"].as_str().expect("the token, once").to_owned();
    assert!(
        token.starts_with(body["prefix"].as_str().expect("prefix")),
        "the token carries its own prefix: {token}"
    );
    assert_eq!(body["name"], json!("Alice's laptop"));
    assert_eq!(
        body["scopes"],
        Value::Null,
        "no subset asked for is no subset, not an empty one"
    );

    let listed = support::get(&kit, "/v1/tokens", &Caller::Token(token.clone())).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
    let tokens = listed.json()["tokens"]
        .as_array()
        .expect("an array")
        .clone();
    assert_eq!(tokens.len(), 1, "{}", listed.text());
    assert_eq!(tokens[0]["prefix"], json!(prefix_of(&token)));
    assert_eq!(tokens[0]["name"], json!("Alice's laptop"));

    // And again: a token is not consumed by being used.
    let again = support::get(&kit, "/v1/tokens", &Caller::Token(token)).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.text());
}

/// Only a signed-in person mints: a token that could mint a token is a
/// credential factory.
#[pollster::test]
async fn a_token_cannot_mint_another_token() {
    let kit = support::signed_in().await;
    let token = mint(&kit, "Alice's laptop", &[]).await;

    for who in [Caller::Token(token), Caller::Anonymous] {
        let refused = support::post_json(
            &kit,
            "/v1/tokens",
            &json!({"name": "A second machine"}),
            &who,
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::UNAUTHORIZED,
            "{}",
            refused.text()
        );
    }
    assert_eq!(rows(&kit).await.len(), 1, "nothing else was minted");
}

/// Creating and revoking are both browser actions, so both gate on the
/// browser's own origin signals the way the grant's approval forms do.
#[pollster::test]
async fn a_cross_site_write_is_refused() {
    let kit = support::signed_in().await;
    let token = mint(&kit, "Alice's laptop", &[]).await;

    let created = support::post_json(
        &kit,
        "/v1/tokens",
        &json!({"name": "Minted from another site"}),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(created.status, StatusCode::FORBIDDEN, "{}", created.text());
    assert!(
        created.text().contains("tokens/cross-site-request"),
        "{}",
        created.text()
    );

    let revoked = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&token)),
        &kit.owner_elsewhere(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::FORBIDDEN, "{}", revoked.text());
    let still = support::get(&kit, "/v1/tokens", &Caller::Token(token)).await;
    assert_eq!(still.status, StatusCode::OK, "{}", still.text());
    assert_eq!(rows(&kit).await.len(), 1, "nothing was minted or revoked");
}

/// A creation cannot be granted a scope its creator does not hold.
#[pollster::test]
async fn a_creation_cannot_reach_a_scope_the_caller_does_not_hold() {
    let kit = support::signed_in().await;

    for asked in ["user:U0SOMEONE", "channel:C0ANY", "everything"] {
        let refused = support::post_json(
            &kit,
            "/v1/tokens",
            &json!({"name": "Too much", "scopes": [asked]}),
            &kit.owner(),
        )
        .await;
        assert_eq!(
            refused.status,
            StatusCode::FORBIDDEN,
            "{asked}: {}",
            refused.text()
        );
        assert!(
            refused.text().contains("tokens/scope-not-held"),
            "{asked}: {}",
            refused.text()
        );
    }
    assert!(rows(&kit).await.is_empty(), "nothing was minted");
}

/// The point of a subset: the token reads the intersection of what its
/// member holds and what it was cut down to, and a token with no subset
/// reads everything its member holds. A narrowed token is also not admin — a
/// subset must not carry workspace-wide rights it does not describe.
#[pollster::test]
async fn a_subset_reads_only_what_is_in_both_grants() {
    let kit = support::signed_in().await;
    let narrow = mint(&kit, "A read-only key", &["shared"]).await;
    let wide = mint(&kit, "A full key", &[]).await;

    assert_eq!(
        rows(&kit).await[0]["scopes"],
        "shared",
        "the subset is stored as asked"
    );

    let held = held_scopes();
    assert!(
        held.len() > 1,
        "and the owner holds more than one scope, or the subset proves nothing: {held:?}"
    );

    let narrow_member = member_of(&kit, &narrow).await;
    let wide_member = member_of(&kit, &wide).await;
    let read = |member: &Member| {
        member
            .read_scopes(Location::Dm, &ChannelMemberships::new())
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        read(&narrow_member),
        vec!["shared".to_owned()],
        "nothing else survives the intersection"
    );
    assert_eq!(
        read(&wide_member),
        held,
        "a token with no subset carries everything its member holds"
    );
    assert!(!narrow_member.is_admin, "a subset is not admin");
    assert!(
        wide_member.is_admin,
        "a full token keeps the member's rights"
    );
}

/// A revoked token is refused by exactly the answer an unknown token is:
/// same status, same body, so probing an endpoint built on this learns
/// nothing about which prefixes exist.
#[pollster::test]
async fn a_revoked_token_answers_exactly_as_an_unknown_one_does() {
    let kit = support::signed_in().await;
    let token = mint(&kit, "Alice's laptop", &[]).await;
    // The right shape, the wrong secret: the one an attacker can produce.
    let unknown = format!("{}_{}", prefix_of(&token), "0".repeat(64));

    let revoked = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&token)),
        &kit.owner(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text());

    let after = support::get(&kit, "/v1/tokens", &Caller::Token(token)).await;
    let never = support::get(&kit, "/v1/tokens", &Caller::Token(unknown)).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED, "{}", after.text());
    assert_eq!(
        after.json(),
        never.json(),
        "a revoked token and an unknown one are the same answer: {}",
        after.text()
    );

    let listed = support::get(&kit, "/v1/tokens", &kit.owner()).await;
    assert_eq!(
        listed.json()["tokens"].as_array().map(Vec::len),
        Some(0),
        "a revoked token is gone from the list: {}",
        listed.text()
    );
}

/// A token whose member has left the workspace stops working, even though
/// the token itself is untouched.
#[pollster::test]
async fn a_token_whose_member_left_stops_working() {
    let kit = support::signed_in().await;
    let token = mint(&kit, "Alice's laptop", &[]).await;

    support::drop_member(&kit, OWNER).await;

    let refused = support::get(&kit, "/v1/tokens", &Caller::Token(token)).await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        refused.text()
    );
}

/// The listing is the caller's own, and a revoke is scoped the same way:
/// another member's prefix is "not found", never "forbidden".
#[pollster::test]
async fn a_token_is_a_members_own_and_nobody_elses() {
    let kit = support::signed_in().await;
    let mine = mint(&kit, "Alice's laptop", &[]).await;
    let theirs = mint_as(&kit, "Bob's build server", &[], &kit.other()).await;

    let listed = support::get(&kit, "/v1/tokens", &kit.owner()).await;
    let tokens = listed.json()["tokens"]
        .as_array()
        .expect("an array")
        .clone();
    assert_eq!(tokens.len(), 1, "only my own: {}", listed.text());
    assert_eq!(tokens[0]["prefix"], json!(prefix_of(&mine)));

    let refused = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&theirs)),
        &kit.owner(),
    )
    .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text());
    let survives = support::get(&kit, "/v1/tokens", &Caller::Token(theirs)).await;
    assert_eq!(survives.status, StatusCode::OK, "{}", survives.text());

    // And a member can revoke their own.
    let mine_gone = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&mine)),
        &kit.owner(),
    )
    .await;
    assert_eq!(mine_gone.status, StatusCode::NO_CONTENT);
}

/// The device flow, end to end, with no browser on the client side: a code,
/// an approval from a person who *is* signed in, and a poll that answers
/// with a credential the CLI can use immediately.
#[pollster::test]
async fn the_device_flow_mints_a_token_the_cli_can_use() {
    let kit = support::signed_in().await;

    let issued = device_flow(&kit, &kit.owner()).await;
    assert_eq!(issued.status, StatusCode::OK, "{}", issued.text());
    let credential = issued.json();
    assert_eq!(credential["token_type"], json!("bearer"));
    let token = credential["access_token"]
        .as_str()
        .expect("the credential")
        .to_owned();

    let listed = support::get(&kit, "/v1/tokens", &Caller::Token(token.clone())).await;
    assert_eq!(listed.status, StatusCode::OK, "{}", listed.text());
    let tokens = listed.json()["tokens"]
        .as_array()
        .expect("an array")
        .clone();
    assert_eq!(tokens.len(), 1, "{}", listed.text());
    assert_eq!(
        tokens[0]["prefix"],
        json!(prefix_of(&token)),
        "the device's token is the one the grant minted"
    );
    assert_eq!(
        tokens[0]["name"],
        json!("Alice's laptop"),
        "labelled with the name the client gave"
    );
    assert_eq!(
        tokens[0]["scopes"],
        Value::Null,
        "the CLI declared no scopes, so the token carries no subset"
    );

    // And it is a token like any other: a browser can revoke it, and the
    // CLI's next call is refused.
    let revoked = support::delete(
        &kit,
        &format!("/v1/tokens/{}", prefix_of(&token)),
        &kit.owner(),
    )
    .await;
    assert_eq!(revoked.status, StatusCode::NO_CONTENT, "{}", revoked.text());
    let refused = support::get(&kit, "/v1/tokens", &Caller::Token(token)).await;
    assert_eq!(
        refused.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        refused.text()
    );
}

/// A visitor with no session is not refused the approval: they are sent to
/// sign in and back to the code they were entering.
#[pollster::test]
async fn an_anonymous_approver_is_sent_to_sign_in() {
    let kit = support::signed_in().await;
    let codes = support::device_code(&kit).await;

    let sent = support::approve(&kit, &codes, &Caller::Anonymous).await;
    assert_eq!(sent.status, StatusCode::SEE_OTHER, "{}", sent.text());
    let location = sent.location().expect("a Location header");
    assert!(
        location.starts_with("/index.html?return_to=") && location.contains("%2Fv1%2Fdevice-auth"),
        "the app's own sign-in page, and the person comes back to the decision they were \
         making: {location}"
    );

    let pending = support::poll(&kit, &codes.device_code).await;
    assert_eq!(
        pending.status,
        StatusCode::BAD_REQUEST,
        "{}",
        pending.text()
    );
    assert_eq!(
        pending.json()["error"],
        json!("authorization_pending"),
        "nobody approved it"
    );
    assert!(rows(&kit).await.is_empty(), "and nothing was minted");
}

/// The token value exists once, in the creation's response. Nowhere else:
/// not in the listing, not in an error, not in the row, not in what the
/// module would print if it were logged.
#[pollster::test]
async fn a_token_is_nowhere_but_the_creation_that_returned_it() {
    let kit = support::signed_in().await;
    // The creation's own response, kept: this is the one place the value is
    // meant to appear, and the last assertion below says so explicitly.
    let created = support::post_json(
        &kit,
        "/v1/tokens",
        &json!({"name": "Alice's laptop"}),
        &kit.owner(),
    )
    .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    let token = created.json()["token"]
        .as_str()
        .expect("the token, once")
        .to_owned();
    let secret = support::secret_of(&token).to_owned();

    let listed = support::get(&kit, "/v1/tokens", &Caller::Token(token.clone())).await;
    assert!(
        !listed.text().contains(&token) && !listed.text().contains(&secret),
        "the listing: {}",
        listed.text()
    );

    let refused = support::delete(&kit, "/v1/tokens/lbp_0000000000000000", &kit.owner()).await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.text());
    assert!(
        !refused.text().contains(&token) && !refused.text().contains(&secret),
        "the 404: {}",
        refused.text()
    );

    for row in rows(&kit).await {
        for (column, stored) in &row {
            assert!(
                !stored.contains(&token) && !stored.contains(&secret),
                "the {column} column carries the token: {stored}"
            );
        }
        assert_ne!(row["secret_hash"], token, "the stored form is the token");
        assert_eq!(
            row["secret_hash"].len(),
            64,
            "sha256 hex: {}",
            row["secret_hash"]
        );
    }

    let printed = format!("{:?}", member_of(&kit, &token).await);
    assert!(
        !printed.contains(&token) && !printed.contains(&secret),
        "a Member's Debug: {printed}"
    );
    assert!(printed.contains(OWNER), "but it does say who: {printed}");

    // The redaction is about printing, not about the browser that asked.
    assert!(
        created.text().contains(&token),
        "the one response that is meant to carry it: {}",
        created.text()
    );
}

/// The two documents a client follows to find all of this.
#[pollster::test]
async fn the_discovery_documents_name_this_deployment() {
    let kit = support::signed_in().await;

    let server = support::get(
        &kit,
        "/.well-known/oauth-authorization-server",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(server.status, StatusCode::OK, "{}", server.text());
    let metadata = server.json();
    assert_eq!(metadata["issuer"], json!(API_BASE));
    assert_eq!(
        metadata["device_authorization_endpoint"],
        json!(format!("{API_BASE}/v1/device-auth/code"))
    );
    assert_eq!(
        metadata["token_endpoint"],
        json!(format!("{API_BASE}/v1/device-auth/token"))
    );
    assert_eq!(
        metadata["grant_types_supported"],
        json!(["urn:ietf:params:oauth:grant-type:device_code"])
    );
    assert_eq!(metadata["response_types_supported"], json!([]));
    assert_eq!(
        metadata["token_endpoint_auth_methods_supported"],
        json!(["none"]),
        "a device client has no secret to present"
    );
    assert!(
        metadata.get("scopes_supported").is_none(),
        "the scopes are per-person, so there is no list to publish: {metadata}"
    );

    let resource = support::get(
        &kit,
        "/.well-known/oauth-protected-resource",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(resource.status, StatusCode::OK, "{}", resource.text());
    let protected = resource.json();
    assert_eq!(
        protected["resource"],
        json!(format!("{API_BASE}/v1/pages/mcp")),
        "the MCP endpoint is the resource"
    );
    assert_eq!(protected["authorization_servers"], json!([API_BASE]));
    assert_eq!(protected["bearer_methods_supported"], json!(["header"]));
}

/// The documents name the host they were fetched from, not a base baked in at
/// build time: one Worker fronts staging, production and `mcp.`, and a client
/// that trusted a wrong `issuer` would follow it somewhere else entirely.
#[pollster::test]
async fn the_discovery_documents_name_the_host_they_were_fetched_from() {
    let kit = support::signed_in().await;

    // `wrangler dev`: an http origin, which only `x-forwarded-proto` reveals.
    let local = support::get_at(
        &kit,
        "/.well-known/oauth-authorization-server",
        "localhost:8787",
        Some("http"),
    )
    .await;
    assert_eq!(local.status, StatusCode::OK, "{}", local.text());
    assert_eq!(local.json()["issuer"], json!("http://localhost:8787"));

    // The MCP host, which is a different name on the same deployment.
    let mcp = support::get_at(
        &kit,
        "/.well-known/oauth-protected-resource",
        "mcp.livingbrain.wiki",
        None,
    )
    .await;
    assert_eq!(mcp.status, StatusCode::OK, "{}", mcp.text());
    let protected = mcp.json();
    assert_eq!(
        protected["authorization_servers"],
        json!(["https://mcp.livingbrain.wiki"]),
        "RFC 9728: a client fetches this from the resource's own host"
    );
    assert_eq!(
        protected["resource"],
        json!("https://mcp.livingbrain.wiki/v1/pages/mcp"),
        "the resource must be under the origin the client reached it on"
    );

    // And the deployment the tests sign into is still itself.
    let staged = support::get(
        &kit,
        "/.well-known/oauth-authorization-server",
        &Caller::Anonymous,
    )
    .await;
    assert_eq!(staged.json()["issuer"], json!(API_BASE));
    assert_ne!(
        API_BASE, "https://api.livingbrain.wiki",
        "the kit's own host must not be the production name, or this proves nothing"
    );
}

// ---------------------------------------------------------------------------
// Helpers

async fn mint(kit: &Kit, name: &str, scopes: &[&str]) -> String {
    mint_as(kit, name, scopes, &kit.owner()).await
}

async fn mint_as(kit: &Kit, name: &str, scopes: &[&str], who: &Caller) -> String {
    let body = if scopes.is_empty() {
        json!({"name": name})
    } else {
        json!({"name": name, "scopes": scopes})
    };
    let created = support::post_json(kit, "/v1/tokens", &body, who).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.text());
    created.json()["token"]
        .as_str()
        .expect("the token, once")
        .to_owned()
}

/// The caller a token resolves to, asked of the same `authenticate` the
/// routes use.
async fn member_of(kit: &Kit, token: &str) -> Member {
    livingbrain_tokens::authenticate(&kit.ports, &Caller::Token(token.to_owned()).headers())
        .await
        .expect("the token authenticates")
}

/// What the owner holds in a direct message, as scope strings.
fn held_scopes() -> Vec<String> {
    scopes_for(
        &UserId::new(OWNER.to_owned()),
        Location::Dm,
        &ChannelMemberships::new(),
    )
    .scope_strings()
    .collect()
}
