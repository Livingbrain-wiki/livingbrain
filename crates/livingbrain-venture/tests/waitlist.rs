//! Issue #23 acceptance, over the venture's own waitlist composition
//! (`livingbrain_venture::waitlist_module`) on `cratefield-testing`'s fakes:
//! joins send exactly one confirmation mail (and none again inside the send
//! cooldown), the CSV export is behind the admin token, and an unknown
//! product is rejected without a send. The kit runs the module's migrations
//! on an in-memory SQLite database, so these exercise the same routes the
//! Worker serves — no network, no Cloudflare.

use cratefield_core::MapConfig;
use cratefield_core::axum::http::header::CONTENT_TYPE;
use cratefield_core::axum::http::{Method, StatusCode};
use cratefield_testing::{TestHarness, request, request_as};
use livingbrain_venture::waitlist_module;
use std::sync::Arc;

const ADMIN: &str = "test-admin-token-0123456789abcdef";
const JOIN: &str = r#"{"email":"nick@example.com","product":"livingbrain","captchaToken":"x"}"#;

/// The venture's composition over the kit: the real `venture()` and
/// `waitlist_module()` (so the derived API host and sender are the ones that
/// deploy), allow-all captcha, a recording mailer, the rate limiter and
/// signer the module requires, and the module's migrations applied. `config`
/// carries the harness-level `ADMIN_TOKEN`.
fn kit(config: MapConfig) -> TestHarness {
    TestHarness::with_builder(
        vec![Box::new(waitlist_module())],
        |builder| {
            builder
                .venture(livingbrain_venture::venture())
                .templates(livingbrain_venture::templates())
        },
        move |ports| ports.config = Arc::new(config),
    )
}

#[pollster::test]
async fn joining_sends_one_confirmation_and_the_cooldown_suppresses_a_repeat() {
    let kit = kit(MapConfig::default());

    let first = request(&kit.router, Method::POST, "/v1/waitlist", Some(JOIN)).await;
    assert_eq!(first.status, StatusCode::ACCEPTED);
    // The join answers before the mail: the send rides the request's `Defer`.
    kit.defer.drain().await;
    assert_eq!(kit.mailer.sent().len(), 1, "exactly one confirmation mail");

    let mail = kit.mailer.last_message().expect("a confirmation mail");
    assert!(mail.to.contains("nick@example.com"), "to: {}", mail.to);
    assert!(
        mail.subject.contains("livingbrain"),
        "subject names the product: {}",
        mail.subject
    );
    assert!(!mail.html.is_empty() && !mail.text.is_empty());

    // A second join of the same address and product inside the cooldown
    // answers identically and sends nothing.
    let repeat = request(&kit.router, Method::POST, "/v1/waitlist", Some(JOIN)).await;
    assert_eq!(repeat.status, StatusCode::ACCEPTED);
    kit.defer.drain().await;
    assert_eq!(kit.mailer.sent().len(), 1, "the repeat must not re-send");
}

/// The `venture()` domain is the apex, so the module derives
/// `https://api.livingbrain.wiki` for the confirm link and
/// `no-reply@send.livingbrain.wiki` for the sender. Passing the API host as
/// the domain would double the label (`api.api…`, `send.api…`); this pins it.
#[pollster::test]
async fn the_confirm_mail_uses_the_apex_derived_api_host_and_sender() {
    let kit = kit(MapConfig::default());
    let response = request(&kit.router, Method::POST, "/v1/waitlist", Some(JOIN)).await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    kit.defer.drain().await;

    let mail = kit.mailer.last_message().expect("a confirmation mail");
    assert!(
        mail.from.ends_with("@send.livingbrain.wiki"),
        "sender is the apex send subdomain, not send.api…: {}",
        mail.from
    );
    assert!(
        mail.text
            .contains("https://api.livingbrain.wiki/v1/waitlist/confirm"),
        "confirm link points at the API host, not api.api…: {}",
        mail.text
    );
}

#[pollster::test]
async fn export_csv_is_gated_by_the_admin_token() {
    let kit = kit(MapConfig::from_pairs([("ADMIN_TOKEN", ADMIN)]));
    request(&kit.router, Method::POST, "/v1/waitlist", Some(JOIN)).await;
    kit.defer.drain().await;

    let path = "/v1/waitlist/admin/export.csv";

    let missing = request(&kit.router, Method::GET, path, None).await;
    assert_eq!(missing.status, StatusCode::UNAUTHORIZED, "no token");

    let wrong = request_as(&kit.router, Method::GET, path, "not-the-token", None).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN, "wrong token");

    let ok = request_as(&kit.router, Method::GET, path, ADMIN, None).await;
    assert_eq!(ok.status, StatusCode::OK);
    assert_eq!(
        ok.headers.get(CONTENT_TYPE).expect("content type"),
        "text/csv; charset=utf-8"
    );
    let body = String::from_utf8(ok.body().to_vec()).expect("utf-8 csv");
    assert!(body.starts_with("id,email,product,status,"), "{body}");
    assert!(
        body.contains("nick@example.com"),
        "the joined address: {body}"
    );
}

#[pollster::test]
async fn an_unknown_product_is_rejected_without_sending() {
    let kit = kit(MapConfig::default());
    let body = r#"{"email":"nick@example.com","product":"someone-else","captchaToken":"x"}"#;

    let response = request(&kit.router, Method::POST, "/v1/waitlist", Some(body)).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    kit.defer.drain().await;
    assert_eq!(kit.mailer.sent().len(), 0, "no mail for an unknown product");
}
