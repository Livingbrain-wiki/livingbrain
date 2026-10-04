//! The venture's branded mail (issue #63): the theme parses and clears WCAG
//! AA, the waitlist's confirm mails and a venture message render the way the
//! snapshots beside this file say, and a send carries both parts.
//!
//! The waitlist module is not mounted yet, so these render its templates
//! directly through `themed_templates` rather than through a harness.

use cratefield_adapter_owlpost::Owlpost;
use cratefield_core::{Mailer, Message, Rendered, SystemClock};
use cratefield_mail_templates::{Email, Message as MailBody};
use cratefield_module_waitlist::themed_templates;
use cratefield_testing::FakeHttpClient;
use livingbrain_venture::mail_theme;
use serde_json::json;
use std::sync::Arc;

/// WCAG 2.1 relative luminance of an `#rrggbb` colour.
fn luminance(hex: &str) -> f64 {
    let channel = |start: usize| {
        let byte = u8::from_str_radix(&hex[start..start + 2], 16).expect("a hex channel");
        let value = f64::from(byte) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
}

/// WCAG 2.1 contrast ratio between two `#rrggbb` colours, 1:1 to 21:1.
fn contrast(a: &str, b: &str) -> f64 {
    let (lighter, darker) = {
        let (a, b) = (luminance(a), luminance(b));
        if a >= b { (a, b) } else { (b, a) }
    };
    (lighter + 0.05) / (darker + 0.05)
}

#[test]
fn the_theme_parses_and_carries_the_brand() {
    let theme = mail_theme();
    assert_eq!(theme.brand_name, "Living Brain");
    assert_eq!(theme.wordmark, "livingbrain.wiki");
    assert_eq!(theme.site_url, "https://livingbrain.wiki");
    assert_eq!(
        theme.logo_url.as_deref(),
        Some("https://livingbrain.wiki/assets/email/logo-64.png")
    );
    assert_eq!(theme.contact.as_deref(), Some("hello@livingbrain.wiki"));
    assert_eq!(theme.light.ink, "#101d21");
    assert_eq!(theme.light.accent, "#00704e");
    assert_eq!(theme.dark.ink, "#e9f0f0");
    assert_eq!(theme.dark.accent, "#56f1b0");
}

#[test]
fn every_text_colour_clears_wcag_aa_on_its_background() {
    let theme = mail_theme();
    for (scheme, palette) in [("light", &theme.light), ("dark", &theme.dark)] {
        for (name, foreground) in [
            ("ink", &palette.ink),
            ("text", &palette.text),
            ("muted", &palette.muted),
            ("accent", &palette.accent),
        ] {
            let ratio = contrast(foreground, &palette.card);
            assert!(ratio >= 4.5, "{scheme} {name} on card is {ratio:.2}:1");
        }
        // Muted also carries the footer, which sits on the page rather than
        // on a card.
        let ratio = contrast(&palette.muted, &palette.bg);
        assert!(ratio >= 4.5, "{scheme} muted on bg is {ratio:.2}:1");
        let ratio = contrast(&palette.button_text, &palette.button);
        assert!(
            ratio >= 4.5,
            "{scheme} button label on button is {ratio:.2}:1"
        );
    }
}

/// The waitlist `confirm` mail in the venture theme. The keys are the
/// module's `ConfirmMailData`.
fn confirm_mail() -> Rendered {
    let templates = themed_templates(&mail_theme());
    let (_, template) = templates
        .iter()
        .find(|(id, _)| id == "waitlist/confirm")
        .expect("waitlist ships a confirm template");
    template
        .render(
            &json!({
                "venture": "Living Brain",
                "product": "Living Brain",
                "email": "ada@example.com",
                "confirm_url": "https://livingbrain.wiki/v1/waitlist/confirm?token=example-token",
            }),
            "en",
        )
        .expect("the confirm template renders its sample data")
}

/// The waitlist `confirmed` mail in the venture theme. The keys are the
/// module's `ConfirmedMailData`.
fn confirmed_mail() -> Rendered {
    let templates = themed_templates(&mail_theme());
    let (_, template) = templates
        .iter()
        .find(|(id, _)| id == "waitlist/confirmed")
        .expect("waitlist ships a confirmed template");
    template
        .render(
            &json!({
                "venture": "Living Brain",
                "product": "Living Brain",
                "email": "ada@example.com",
                "position": 42,
                "status_url": "https://livingbrain.wiki/v1/waitlist/status?token=example-token",
            }),
            "en",
        )
        .expect("the confirmed template renders its sample data")
}

/// A short digest-style message through the venture theme: the shape the
/// venture's own mail will take (there is none yet, only the waitlist's).
fn venture_message() -> Email {
    MailBody::new("Living Brain this week", "This week on Living Brain")
        .preheader("Three pages changed and one colony landed a pull request.")
        .paragraph("The wiki gained three pages, and a colony landed a pull request.")
        .button("Open Living Brain", "https://livingbrain.wiki")
        .fallback_link()
        .link_intro("If the button does not work, open this link:")
        .recipient("ada@example.com")
        .why("you have a Living Brain account")
        .render(&mail_theme())
}

#[test]
fn the_waitlist_confirm_mail_renders() {
    let mail = confirm_mail();
    insta::assert_snapshot!("waitlist_confirm_subject", mail.subject);
    insta::assert_snapshot!("waitlist_confirm_html", mail.html);
    insta::assert_snapshot!("waitlist_confirm_text", mail.text);
}

#[test]
fn the_waitlist_confirmed_mail_renders() {
    let mail = confirmed_mail();
    insta::assert_snapshot!("waitlist_confirmed_subject", mail.subject);
    insta::assert_snapshot!("waitlist_confirmed_html", mail.html);
    insta::assert_snapshot!("waitlist_confirmed_text", mail.text);
}

#[test]
fn a_venture_message_renders_in_the_theme() {
    let mail = venture_message();
    insta::assert_snapshot!("venture_message_subject", mail.subject);
    insta::assert_snapshot!("venture_message_html", mail.html);
    insta::assert_snapshot!("venture_message_text", mail.text);
}

#[test]
fn a_sent_mail_carries_html_and_text() {
    let http = FakeHttpClient::ok_json(r#"{"id":"msg_example"}"#);
    let adapter = Owlpost::new(
        Arc::new(http.clone()),
        Arc::new(SystemClock),
        Some("test-key".to_owned()),
        "Living Brain <hello@livingbrain.wiki>",
        None,
    );
    let mail = venture_message();
    let message = Message::new(
        "ada@example.com",
        // Empty: the adapter's own `from` stands in for a send that does not
        // name one, the way a caller with no per-message override relies on.
        "",
        &mail.subject,
        &mail.text,
        &mail.html,
    );

    let outcome = pollster::block_on(adapter.send(message)).expect("the fake accepts the send");
    assert!(
        matches!(outcome, cratefield_core::SendOutcome::Sent { .. }),
        "{outcome:?}"
    );

    let captured = http.captured();
    assert_eq!(captured.len(), 1, "one request, recorded: {captured:?}");
    let (method, uri, body) = &captured[0];
    assert_eq!(method, "POST");
    assert!(uri.ends_with("/v1/emails"), "{uri}");
    let body: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
    assert!(
        body["html"].as_str().is_some_and(|html| !html.is_empty()),
        "the request has no HTML part: {body}"
    );
    assert!(
        body["text"].as_str().is_some_and(|text| !text.is_empty()),
        "the request has no text part: {body}"
    );
}
