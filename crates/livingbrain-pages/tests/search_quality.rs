//! Issue #281 in tests: what a search answers, and when it says it does not
//! know.
//!
//! Three promises, each over a synthetic brain (no production data):
//!
//! - **All words first, else some words above a floor.** A query is answered
//!   by the pages holding every word; only when none does, by the pages
//!   holding enough of its weight, and otherwise by an explicit "no
//!   confident match".
//! - **Sessions are out of a default search.** A coding-agent transcript
//!   (`agent-log-*`) is found only when the caller opts in.
//! - **A golden set.** Representative questions, each marked as a hit (the
//!   brain answers it), a partial (it answers part of it) or a miss (nothing
//!   in the brain answers it), and what each must return.

mod common;

use std::sync::Arc;

use cratefield_core::UlidIdGen;
use cratefield_testing::{FixedClock, MemoryBlob, TestHarness};
use livingbrain_pages::access::ScopeFilter;
use livingbrain_pages::{
    Author, EntityType, MatchMode, PageStore, PageWrite, Pages, SearchOptions, SearchOutcome,
};

const SCOPE: &str = "team";

/// The synthetic brain: slug, title, body. Pages a person wrote, then two
/// coding-agent sessions, the second long enough to be a transcript.
fn fixture() -> Vec<(String, String, String)> {
    let page =
        |slug: &str, title: &str, body: &str| (slug.to_owned(), title.to_owned(), body.to_owned());
    let transcript: String = (0..120)
        .map(|i| {
            format!(
                "Turn {i}: the agent read the earthos codebase, listed the lockfile issues \
                 and ran the release pipeline locally; output line {i} of the build log.\n"
            )
        })
        .collect();
    vec![
        page(
            "release-pipeline",
            "Release pipeline",
            "Config artifacts are released by release-please and promoted by tag through \
             staging to production. A rollback re-runs the previous release.",
        ),
        page(
            "backup-restore",
            "Backups",
            "Backups ship WAL segments to the backup VM. A restore replays them with \
             point-in-time recovery.",
        ),
        page(
            "vault-secrets",
            "Vault secrets",
            "Runtime secrets are rendered by Vault Agent into the containers. Rotation is \
             a manual step that needs the unseal keys.",
        ),
        page(
            "kafka-topics",
            "Kafka topics",
            "Ingestion runs on Kafka topics. Retention on the ingest topic is seven days.",
        ),
        page(
            "billing-invoices",
            "Billing",
            "Invoices are issued monthly. Refunds go back through the payment processor.",
        ),
        page(
            "ticket-scanning",
            "Ticket scanning",
            "Tickets are scanned at the gate with a phone camera, offline when needed.",
        ),
        page(
            "token-balances",
            "Token balances",
            "A token is a prepaid balance spent on model calls. Balances never expire.",
        ),
        page(
            "onboarding",
            "Onboarding",
            "New members join a workspace from an invitation and pick their first project.",
        ),
        page(
            "agent-log-earthos",
            "Coding session, earthos",
            &format!("Session transcript.\n{transcript}"),
        ),
        page(
            "agent-log-chi-release",
            "Coding session, release",
            "The session promoted a release tag and checked the release pipeline.",
        ),
    ]
}

/// A store over the fixture, written in one pass, with a clock that orders
/// the writes so recency is deterministic.
fn brain() -> (PageStore, Vec<String>) {
    let pages: Vec<(String, String, String)> = fixture();
    brain_of(
        &pages
            .iter()
            .map(|(slug, title, body)| (slug.as_str(), title.as_str(), body.as_str()))
            .collect::<Vec<_>>(),
    )
}

/// A store over the given `(slug, title, body)` pages in [`SCOPE`].
fn brain_of(pages: &[(&str, &str, &str)]) -> (PageStore, Vec<String>) {
    let kit = TestHarness::all_dialects(|| vec![Box::new(Pages::new())])
        .into_iter()
        .next()
        .expect("sqlite is always available");
    let blob = Arc::new(MemoryBlob::new());
    let store = PageStore::new(
        kit.db.clone(),
        blob,
        common::kms(),
        Arc::new(FixedClock(kit.clock.0)),
        Arc::new(UlidIdGen),
    );
    let mut slugs = Vec::new();
    pollster::block_on(async {
        for &(slug, title, body) in pages {
            store
                .write(
                    SCOPE,
                    slug,
                    PageWrite {
                        entity_type: EntityType::Project,
                        markdown: format!("---\nname: {title}\n---\n{body}"),
                        author: Author::Human {
                            id: "u-a".to_owned(),
                        },
                        base_version: None,
                    },
                )
                .await
                .unwrap_or_else(|error| panic!("write {slug}: {error}"));
            slugs.push(slug.to_owned());
        }
    });
    (store, slugs)
}

fn filters() -> Vec<ScopeFilter> {
    vec![ScopeFilter::whole(SCOPE, "")]
}

fn search(store: &PageStore, query: &str, include_sessions: bool) -> SearchOutcome {
    pollster::block_on(store.search_outcome(
        &filters(),
        query,
        10,
        SearchOptions {
            include_sessions,
            ..SearchOptions::default()
        },
    ))
    .expect("a search")
}

fn slugs_of(outcome: &SearchOutcome) -> Vec<&str> {
    outcome.hits.iter().map(|hit| hit.slug.as_str()).collect()
}

#[test]
fn every_word_comes_first_and_the_fallback_only_runs_when_nothing_holds_them_all() {
    let (store, _) = brain();
    // Both words are on one page: an all-words match, and nothing else is
    // returned even though other pages hold one of the words.
    let both = search(&store, "release promoted", false);
    assert_eq!(both.mode, Some(MatchMode::All));
    assert_eq!(slugs_of(&both), ["release-pipeline"]);

    // No page holds both `vault` and `kafka`: the fallback answers, each
    // page that holds half the query's weight.
    let split = search(&store, "vault kafka", false);
    assert_eq!(split.mode, Some(MatchMode::Any));
    assert!(slugs_of(&split).contains(&"vault-secrets"), "{split:?}");
    assert!(slugs_of(&split).contains(&"kafka-topics"), "{split:?}");
}

#[test]
fn a_page_holding_only_a_common_word_is_below_the_floor() {
    // Three pages: `widget` is on two of them (common), `kafka` on one (rare).
    // A question for both words is answered by the page holding the rare one.
    // The page holding only the common word is below the floor: it holds a
    // third of the query's weight, and it needs half.
    let (store, _) = brain_of(&[
        ("alpha-one", "Alpha one", "A widget and a gadget."),
        ("alpha-two", "Alpha two", "Another widget, nothing else."),
        ("beta", "Beta", "Kafka ingests the events."),
    ]);
    let outcome = search(&store, "widget kafka", false);
    assert_eq!(outcome.mode, Some(MatchMode::Any), "{outcome:?}");
    assert_eq!(slugs_of(&outcome), ["beta"], "{outcome:?}");
}

#[test]
fn a_query_with_nothing_in_the_brain_is_an_explicit_no_confident_match() {
    let (store, _) = brain();
    let outcome = search(&store, "quantum entanglement in the kitchen", false);
    assert_eq!(outcome.mode, None);
    assert!(outcome.hits.is_empty());
    // Stopwords only is the same answer: there is no word to search for.
    assert_eq!(
        search(&store, "what is it", false),
        SearchOutcome::no_confident_match()
    );
}

#[test]
fn sessions_are_out_of_a_default_search_and_in_with_the_opt_in() {
    let (store, _) = brain();
    // `lockfile` and `earthos` are only in a session transcript.
    let by_default = search(&store, "earthos lockfile", false);
    assert!(
        slugs_of(&by_default)
            .iter()
            .all(|slug| !slug.starts_with("agent-log-")),
        "a session leaked into a default search: {by_default:?}"
    );
    assert_eq!(by_default.mode, None, "{by_default:?}");

    let opted_in = search(&store, "earthos lockfile", true);
    assert_eq!(opted_in.mode, Some(MatchMode::All));
    assert_eq!(slugs_of(&opted_in), ["agent-log-earthos"]);

    // A session is still a page: a word only on a person's page finds that
    // page either way, and the session does not crowd it out by default.
    let release = search(&store, "release pipeline", false);
    assert!(slugs_of(&release).contains(&"release-pipeline"));
    assert!(!slugs_of(&release).contains(&"agent-log-chi-release"));
}

/// The golden set: representative questions over the synthetic brain, as
/// hit, partial or miss, with what each must answer.
#[test]
fn the_golden_set_answers_the_hits_and_abstains_on_the_misses() {
    let (store, _) = brain();
    // (question, kind, the page it must find first; None for a miss)
    let golden: &[(&str, &str, Option<&str>)] = &[
        (
            "How are config releases promoted to production?",
            "hit",
            Some("release-pipeline"),
        ),
        (
            "Where do the WAL backups get restored from?",
            "hit",
            Some("backup-restore"),
        ),
        (
            "What does a Vault secret rotation involve?",
            "hit",
            Some("vault-secrets"),
        ),
        (
            "How long is Kafka topic retention for ingestion?",
            "hit",
            Some("kafka-topics"),
        ),
        (
            "What is the refund policy for invoices?",
            "hit",
            Some("billing-invoices"),
        ),
        (
            "How do tickets get scanned at the gate?",
            "hit",
            Some("ticket-scanning"),
        ),
        (
            "Do token balances ever expire?",
            "hit",
            Some("token-balances"),
        ),
        (
            "How does a new member join a workspace?",
            "hit",
            Some("onboarding"),
        ),
        // Part of the question is in the brain: the release rollback words
        // are on the release page, `kubernetes` is nowhere.
        (
            "release rollback on kubernetes",
            "partial",
            Some("release-pipeline"),
        ),
        // A question the brain does not hold.
        ("best pizza topping in Reykjavik", "miss", None),
        ("quantum entanglement in the kitchen", "miss", None),
        ("what were the ones that it was", "miss", None),
    ];

    let mut failures = Vec::new();
    for (question, kind, expected) in golden {
        let outcome = search(&store, question, false);
        let ok = match expected {
            Some(slug) => {
                outcome.hits.first().map(|hit| hit.slug.as_str()) == Some(*slug)
                    && outcome.mode.is_some()
            }
            None => outcome.mode.is_none(),
        };
        if !ok {
            failures.push(format!("{kind}: {question:?} -> {outcome:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "golden set regressions:\n{}",
        failures.join("\n")
    );

    // A partial is answered under the fallback, not as an all-words match.
    let partial = search(&store, "release rollback on kubernetes", false);
    assert_eq!(partial.mode, Some(MatchMode::Any));
}
