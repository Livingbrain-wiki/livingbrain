//! Issue #61's rule for transactional mail, written down so it cannot drift:
//!
//! > Once transactional mail actually goes through Owlpost (it is **planned**
//! > today), add a short "Sent with Owlpost" line, linking to
//! > https://owlpost.to/, to the footer of every transactional email.
//! > **Not before: until then mail is not sent with Owlpost and the footer
//! > would be false.**
//!
//! The Factory Zero stack registry — vendored at `crates/livingbrain-stack`,
//! published at https://factory0.ventures/stack.json — is the source of truth
//! for whether Owlpost is actually sending. So the footer carries the line if
//! and only if that registry's `email` role reads `"status": "live"`. Change
//! this test when the *registry* changes, never to make it pass.

use livingbrain_venture::mail_theme;

/// `include_str!` rather than a filesystem read: a Worker has no filesystem to
/// read, and `tests/redact.rs` sets the precedent. It is relative to this file,
/// and needs no dependency and no lockfile entry to reach the registry.
const REGISTRY: &str = include_str!("../../livingbrain-stack/src/stack.json");

#[test]
fn the_footer_names_owlpost_exactly_when_the_registry_says_owlpost_is_live() {
    let registry: serde_json::Value =
        serde_json::from_str(REGISTRY).expect("the vendored stack registry is JSON");
    let entry = registry["uses"]
        .as_array()
        .expect("the registry lists what the venture uses")
        .iter()
        .find(|entry| entry["role"] == "email")
        .expect("the registry has an `email` role");
    let owlpost_is_live = entry["status"] == "live";

    // `MailTheme::footer` is rendered verbatim into every mail's footer
    // (`cratefield-mail-templates`, `write_footer`), empty meaning no lines.
    let footer_names_owlpost = mail_theme()
        .footer
        .iter()
        .any(|line| line.contains("Sent with Owlpost"));

    assert_eq!(
        owlpost_is_live, footer_names_owlpost,
        "the mail theme's footer carries 'Sent with Owlpost' only when the \
         registry says Owlpost is live; today the `email` role is {:?}",
        entry["status"]
    );
}
