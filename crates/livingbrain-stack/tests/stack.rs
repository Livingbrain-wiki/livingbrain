//! Integration tests: the vendored registry entry, and the two renderers.
//!
//! The drift guard at the bottom is the important one. Everything else here
//! can be true of an entry someone edited by hand; that test cannot.

use livingbrain_stack::{Stack, Status, stack};

/// (a) The vendored document is the FZ-018 entry, with its nine entries in the
/// registry's order.
#[test]
fn the_vendored_entry_is_fz018_with_nine_uses() {
    let stack = stack();
    assert_eq!(stack.venture.id, "FZ-018");
    assert_eq!(stack.venture.name, "Living Brain");
    assert_eq!(stack.source, "https://factory0.ventures/stack.json");
    assert_eq!(
        stack.subprocessors,
        "https://factory0.ventures/ventures/living-brain/"
    );
    let roles: Vec<&str> = stack.entries().iter().map(|e| e.role.as_str()).collect();
    assert_eq!(
        roles,
        [
            "framework",
            "agents",
            "email",
            "support",
            "bug-reports",
            "payments",
            "security-screening",
            "deploys",
            "hosting",
        ],
        "the vendored entry is not the registry's FZ-018 entry"
    );
}

/// (b) Exactly one entry is live, and it is Cloudflare's hosting.
#[test]
fn one_entry_is_live_and_it_is_hosting() {
    let live = stack().live();
    let names: Vec<&str> = live.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(names, ["Cloudflare"], "{names:?}");
    assert_eq!(live[0].role, "hosting");
    assert!(live[0].url.starts_with("https://"));
}

/// (c) Every entry is complete, and every link is one a reader can be sent to.
#[test]
fn every_entry_is_complete_and_https() {
    for entry in stack().entries() {
        for (field, value) in [
            ("phrase", entry.phrase.as_str()),
            ("name", entry.name.as_str()),
            ("url", entry.url.as_str()),
            ("note", entry.note.as_str()),
            ("kind", entry.kind.as_str()),
            ("id", entry.id.as_str()),
        ] {
            assert!(
                !value.trim().is_empty(),
                "{} has an empty {field}",
                entry.role
            );
        }
        assert!(
            entry.url.starts_with("https://"),
            "{}: {} is not https",
            entry.role,
            entry.url
        );
    }
    // Both statuses the registry defines are spelled for, so no entry can
    // arrive with a status nothing renders.
    for status in Status::ALL {
        assert!(!status.gloss(&stack().statuses).is_empty(), "{status:?}");
    }
}

/// (d) The text names every product and links every one of them — and no
/// planned entry is ever presented as live.
#[test]
fn the_text_shows_every_product_and_mislabels_nothing() {
    let stack = stack();
    let text = stack.text();
    for entry in stack.entries() {
        assert!(text.contains(&entry.name), "missing {}", entry.name);
        assert!(text.contains(&entry.url), "missing {}", entry.url);
        let row = text
            .lines()
            .find(|line| line.contains(&entry.name))
            .unwrap_or_else(|| panic!("no row for {}", entry.name));
        assert!(
            row.contains(&format!(" {} ", entry.status.as_str())),
            "{} is rendered as {:?}, not {:?}: {row}",
            entry.name,
            entry.status,
            Status::ALL.into_iter().find(|s| *s != entry.status),
        );
        if !entry.is_live() {
            assert!(
                !row.contains(" live "),
                "{} is planned but its row reads live: {row}",
                entry.name
            );
        }
    }
    // The wording the list exists for: a planned thing is called planned in
    // the row, not only in a legend at the bottom.
    let cratefield = text
        .lines()
        .find(|line| line.contains("Cratefield"))
        .expect("the framework row");
    assert!(cratefield.contains("planned"), "{cratefield}");
}

/// (e) The two places a reader is sent to check the list against the registry
/// are both in the output.
#[test]
fn the_text_links_the_subprocessors_and_the_registry() {
    let stack = stack();
    for rendered in [stack.text(), stack.markdown()] {
        assert!(rendered.contains(&stack.subprocessors), "{rendered}");
        assert!(rendered.contains(&stack.source), "{rendered}");
        assert!(
            rendered.contains("https://factory0.ventures/ventures/living-brain/"),
            "the subprocessors list is not linked"
        );
        // The registry names no subprocessors page of its own, so the label has
        // to say where the list actually is rather than claim a dedicated page
        // that does not exist.
        assert!(
            rendered.contains("venture page; no privacy page yet"),
            "the subprocessors link is not framed honestly: {rendered}"
        );
    }
}

/// (f) The Markdown carries a link for every entry, so a page can render the
/// list without going through the text renderer.
#[test]
fn the_markdown_links_every_entry() {
    let stack = stack();
    let markdown = stack.markdown();
    for entry in stack.entries() {
        let link = format!("[{}]({})", entry.name, entry.url);
        assert!(markdown.contains(&link), "missing {link}");
        assert!(
            markdown.contains(&format!("**{}**", entry.phrase)),
            "missing the {} phrase",
            entry.phrase
        );
    }
}

/// The registry's U+2019 in the Owlpost note, byte for byte. A vendored file
/// that has been through an ASCII-only editor is a vendored file that no
/// longer matches the registry.
#[test]
fn the_typographic_quote_survived_the_vendoring() {
    assert!(
        stack().entries()[2].note.contains('\u{2019}'),
        "the Owlpost note lost its U+2019"
    );
    assert!(!livingbrain_stack::source_text().contains("brain's"));
}

/// The drift guard.
///
/// When the registry entry changes, re-vendor `stack.json` AND update this
/// digest. Nothing else edits the file: the registry is the source of truth
/// and this constant is the tripwire for an edit that skipped it.
///
/// SHA-256 over the vendored file's bytes, computed as
/// `sha256sum crates/livingbrain-stack/src/stack.json`.
#[test]
fn the_vendored_file_has_not_been_edited_by_hand() {
    use sha2::{Digest, Sha256};
    const PINNED: &str = "9927a43ebd0b447da37390ebbad9be9b89f8d93fff62acfe31e62e6f32710796";

    let digest = format!(
        "{:x}",
        Sha256::digest(livingbrain_stack::source_text().as_bytes())
    );
    assert_eq!(
        digest, PINNED,
        "stack.json drifted: re-vendor it from https://factory0.ventures/stack.json and update this digest"
    );
}

/// The parse is loud, not silent: a malformed document is an error rather than
/// a default list, so a bad re-vendor cannot render as "nothing is in use".
#[test]
fn a_malformed_document_is_an_error() {
    assert!(Stack::from_json("{").is_err());
    assert!(Stack::from_json(r#"{"uses":[]}"#).is_err());
    assert!(Stack::from_json(r#"{"venture":{},"statuses":{},"uses":[]}"#).is_err());
}
