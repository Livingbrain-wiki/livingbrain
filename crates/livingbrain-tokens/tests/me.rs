//! `GET /v1/me` (ADR 0004): "connected as", for a bearer token as well as a
//! session, and what removing a member does to their tokens.

mod support;

use cratefield_core::axum::http::StatusCode;
use serde_json::json;
use support::*;

const ME: &str = "/v1/me";

async fn mint(kit: &Kit, scopes: &[&str]) -> String {
    let made = post_json(
        kit,
        "/v1/tokens",
        &json!({"name": "laptop", "scopes": scopes}),
        &kit.owner(),
    )
    .await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.text());
    made.json()["token"].as_str().expect("a token").to_owned()
}

#[pollster::test]
async fn a_session_reads_who_it_is() {
    let kit = signed_in().await;
    let me = get(&kit, ME, &kit.owner()).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text());
    let body = me.json();
    assert_eq!(body["workspace"]["id"], WORKSPACE);
    assert_eq!(body["principal"]["kind"], "member");
    assert_eq!(body["principal"]["id"], OWNER);
    assert_eq!(body["principal"]["role"], "owner");
    assert_eq!(body["credential"]["kind"], "session");
}

#[pollster::test]
async fn a_token_reads_who_it_is_and_never_its_secret() {
    let kit = signed_in().await;
    let token = mint(&kit, &["shared"]).await;
    let me = get(&kit, ME, &Caller::Token(token.clone())).await;
    assert_eq!(me.status, StatusCode::OK, "{}", me.text());
    let body = me.json();
    assert_eq!(body["principal"]["id"], OWNER);
    assert_eq!(body["credential"]["kind"], "personal_access_token");
    assert_eq!(body["credential"]["prefix"], prefix_of(&token));
    assert_eq!(body["credential"]["scopes"], json!(["shared"]));
    assert_eq!(
        body["principal"]["is_admin"], false,
        "a narrowed token is not admin"
    );
    assert!(
        !me.text().contains(secret_of(&token)),
        "the secret never comes back"
    );
}

#[pollster::test]
async fn nobody_and_a_removed_member_get_the_same_401() {
    let kit = signed_in().await;
    assert_eq!(
        get(&kit, ME, &Caller::Anonymous).await.status,
        StatusCode::UNAUTHORIZED
    );
    let token = mint(&kit, &[]).await;
    drop_member(&kit, OWNER).await;
    let me = get(&kit, ME, &Caller::Token(token)).await;
    assert_eq!(me.status, StatusCode::UNAUTHORIZED);
}

#[pollster::test]
async fn removing_a_member_revokes_every_token_they_hold() {
    let kit = signed_in().await;
    let first = mint(&kit, &[]).await;
    let second = mint(&kit, &["shared"]).await;
    let db = kit.harness.db.clone();
    let revoked =
        livingbrain_tokens::revoke_member_tokens(&*db, WORKSPACE, OWNER, "2026-02-01T00:00:00Z")
            .await
            .expect("revokes");
    assert_eq!(revoked, 2);
    for token in [first, second] {
        assert_eq!(
            get(&kit, ME, &Caller::Token(token)).await.status,
            StatusCode::UNAUTHORIZED,
            "a revoked token is refused even while the member row is live"
        );
    }
    // Another member's tokens are not touched.
    assert_eq!(
        livingbrain_tokens::revoke_member_tokens(&*db, WORKSPACE, OTHER, "2026-02-01T00:00:00Z")
            .await
            .expect("revokes"),
        0
    );
}
