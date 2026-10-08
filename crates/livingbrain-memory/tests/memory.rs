//! Acceptance tests for the memory model (issue #54).

use livingbrain_access::{
    ChannelId, ChannelMemberships, Location, Scope, ScopeSet, UserId, scopes_for,
};
use livingbrain_memory::temporal::{DAY, days_from_civil, iso_date};
use livingbrain_memory::{
    Fact, FactKind, MemoryError, MemoryStore, Observation, RecallQuery, Recalled, Reflection,
    Strategy, questions_match,
};
use proptest::prelude::*;

/// Noon on 15 June 2026 — the "now" every test shares.
fn now() -> i64 {
    days_from_civil(2026, 6, 15) * DAY + 12 * 3_600
}

/// A timestamp on a calendar day, so a test reads as a date and not a number.
fn on(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day) * DAY + 9 * 3_600
}

/// Ada's scopes in a DM, a member of `channels`.
fn ada_in(channels: &[&str]) -> ScopeSet {
    let mut memberships = ChannelMemberships::new();
    for channel in channels {
        memberships.sync_channel(
            ChannelId::new((*channel).into()),
            [UserId::new("ada".into())],
        );
    }
    scopes_for(&UserId::new("ada".into()), Location::Dm, &memberships)
}

/// Ada's scopes in a DM with the one private channel most of these tests use.
fn ada_scopes() -> ScopeSet {
    ada_in(&["c1"])
}

fn shared() -> Scope {
    Scope::Shared
}

fn ada() -> Scope {
    Scope::User(UserId::new("ada".into()))
}

fn channel(id: &str) -> Scope {
    Scope::Channel(ChannelId::new(id.into()))
}

fn fact(scope: Scope, subject: &str, predicate: &str, object: &str, quote: &str, at: i64) -> Fact {
    Fact::new(FactKind::World, scope, subject, predicate, object, at)
        .quote(quote)
        .source("thread:t1")
}

fn keep(store: &mut MemoryStore, fact: Fact) -> String {
    store.retain(fact).expect("a clean fact")
}

fn observations(store: &MemoryStore, scopes: &ScopeSet) -> Vec<Observation> {
    store.observations(scopes).into_iter().cloned().collect()
}

/// One fact said three times in three threads is one belief with three pieces
/// of evidence — and a later contradiction revises it rather than replacing
/// it.
#[test]
fn restatement_consolidates_and_contradiction_revisions() {
    let mut store = MemoryStore::new();
    let quote = "ada is the on-call engineer for the payments service";
    for index in 0..3u32 {
        keep(
            &mut store,
            Fact::new(
                FactKind::World,
                ada(),
                "Ada",
                "on-call engineer for",
                "payments",
                on(2026, 3, index + 1),
            )
            .quote(quote)
            .source(&format!("thread:t{index}")),
        );
    }

    store.consolidate();
    let found = observations(&store, &ada_scopes());
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].proof_count, 3);
    assert_eq!(found[0].belief, "payments");
    // Three restatements, three sources, one belief — and the citations are
    // the words as they were written, not a summary of them.
    let cited: Vec<&str> = found[0]
        .evidence
        .iter()
        .map(|evidence| evidence.quote.as_str())
        .collect();
    assert_eq!(cited, [quote; 3]);
    let sources: Vec<&str> = found[0]
        .evidence
        .iter()
        .map(|evidence| evidence.source.as_str())
        .collect();
    assert_eq!(sources, ["thread:t0", "thread:t1", "thread:t2"]);

    // Running the nightly twice does nothing the second time.
    store.consolidate();
    assert_eq!(observations(&store, &ada_scopes())[0], found[0]);

    // A later fact saying something else refines the belief.
    keep(
        &mut store,
        Fact::new(
            FactKind::World,
            ada(),
            "ada",
            "on-call engineer for",
            "billing",
            on(2026, 5, 1),
        )
        .quote("bob took the payments rotation from ada in May")
        .source("thread:t9"),
    );
    store.consolidate();

    let observation = &observations(&store, &ada_scopes())[0];
    assert_eq!(observation.belief, "billing");
    assert_eq!(observation.proof_count, 1);
    assert_eq!(observation.history.len(), 1);
    assert_eq!(observation.history[0].belief, "payments");
    assert_eq!(observation.history[0].evidence.len(), 3);
    assert_eq!(observation.history[0].superseded_at, on(2026, 5, 1));

    store.consolidate();
    let idempotent = &observations(&store, &ada_scopes())[0];
    assert_eq!(idempotent.history.len(), 1, "a third run is still a no-op");
    assert_eq!(idempotent.proof_count, 1);
}

/// Claiming a superseded belief again revives it, with everything that
/// supported it before; two facts in the same second fold in the order they
/// were retained.
#[test]
fn a_revived_belief_carries_its_old_evidence() {
    let mut store = MemoryStore::new();
    for (object, day) in [("a", 1), ("b", 2), ("a", 3)] {
        keep(
            &mut store,
            fact(
                ada(),
                "the sky",
                "is",
                object,
                "the sky is a colour of blue",
                on(2026, 3, day),
            ),
        );
    }

    let same_second = on(2026, 3, 1);
    // Two facts sharing an instant: retention order decides, not the id.
    keep(
        &mut store,
        fact(
            ada(),
            "the tide",
            "is",
            "first",
            "the tide is high",
            same_second,
        ),
    );
    keep(
        &mut store,
        fact(
            ada(),
            "the tide",
            "is",
            "second",
            "the tide is low",
            same_second,
        ),
    );

    store.consolidate();
    let found = observations(&store, &ada_scopes());
    let sky = found
        .iter()
        .find(|observation| observation.key.subject == "the sky")
        .expect("the sky observation");
    assert_eq!(sky.belief, "a", "the third claim revives the first");
    assert_eq!(sky.proof_count, 2, "and counts both of its proofs");
    assert_eq!(sky.history.len(), 1);
    assert_eq!(sky.history[0].belief, "b");

    let tide = found
        .iter()
        .find(|observation| observation.key.subject == "the tide")
        .expect("the tide observation");
    assert_eq!(tide.belief, "second");
}

/// A model answers from storage, goes stale when its evidence moves, and is
/// refreshed with a new citation.
#[test]
fn a_mental_model_is_answered_refreshed_and_goes_stale() {
    let mut store = MemoryStore::new();
    store.set_now(now());
    let scopes = ada_scopes();
    keep(
        &mut store,
        Fact::new(
            FactKind::World,
            ada(),
            "Ada",
            "lives in",
            "Amsterdam",
            on(2026, 1, 4),
        )
        .quote("ada moved to amsterdam in january")
        .source("thread:homes"),
    );

    let id = store.define_model(ada(), "Where does Ada live?");
    let mut seen = 0;
    store
        .refresh_model(&id, &scopes, |recalled: &[Recalled]| {
            seen = recalled.len();
            format!("Ada lives in {} — {}", recalled.len(), recalled[0].quote)
        })
        .expect("the asker may read this model")
        .expect("a stored answer");
    assert!(seen > 0, "the writer is handed the recalled evidence");

    let answer = store.answer_model(&id, &scopes).expect("an answer");
    assert!(answer.tokens_used > 0);
    assert!(!answer.stale);
    assert_eq!(answer.citations.len(), 1);

    // New evidence in the model's own scope, saying the same subject.
    let new = keep(
        &mut store,
        Fact::new(
            FactKind::World,
            ada(),
            "Ada",
            "lives in",
            "Rotterdam",
            on(2026, 6, 1),
        )
        .quote("ada moved to rotterdam, where ada works")
        .source("thread:move"),
    );

    assert_eq!(store.stale_models(&scopes), vec![id.clone()]);
    assert!(store.answer_model(&id, &scopes).expect("an answer").stale);

    store
        .refresh_model(&id, &scopes, |recalled: &[Recalled]| {
            format!("Ada lives in Rotterdam, per {} facts", recalled.len())
        })
        .expect("the asker may read this model");
    let answer = store.answer_model(&id, &scopes).expect("an answer");
    assert!(!answer.stale);
    assert!(store.stale_models(&scopes).is_empty());
    assert!(
        answer.citations.contains(&new),
        "the refreshed answer cites the new evidence: {answer:?}"
    );
}

/// A model's evidence is its own scope, whatever the refreshing asker may
/// read — and an asker who cannot read the model at all is refused.
#[test]
fn a_model_is_refreshed_only_from_its_own_scope() {
    let mut store = MemoryStore::new();
    store.set_now(now());
    let wide = ada_in(&["c1", "c2"]);
    let narrow = ada_in(&["c1"]);
    let private = keep(
        &mut store,
        fact(
            channel("c1"),
            "Ada",
            "lives in",
            "Amsterdam",
            "ada lives in amsterdam, only c1 knows",
            on(2026, 1, 4),
        ),
    );
    let foreign = keep(
        &mut store,
        fact(
            channel("c2"),
            "Ada",
            "lives in",
            "Rotterdam",
            "ada lives in rotterdam, only c2 knows",
            on(2026, 6, 1),
        ),
    );

    let id = store.define_model(channel("c1"), "Where does Ada live?");
    let stored = store
        .refresh_model(&id, &wide, |recalled: &[Recalled]| {
            format!("Ada lives somewhere, per {} facts", recalled.len())
        })
        .expect("c1 is readable")
        .expect("a stored answer");
    assert_eq!(
        stored.citations.as_slice(),
        [private.as_str()],
        "c2 was readable, not drawable"
    );

    // And the narrower asker is served an answer with nothing of c2's in it.
    match store.reflect(
        &narrow,
        &RecallQuery::new("where does ada live", 200, now()),
    ) {
        Reflection::Model(answer) => assert_eq!(answer.citations.as_slice(), [private.as_str()]),
        other => panic!("expected the stored model: {other:?}"),
    }

    // An asker with no c1 may neither refresh it nor read it back.
    let outsider = ada_in(&["c2"]);
    assert!(matches!(
        store.refresh_model(&id, &outsider, |_| String::new()),
        Err(MemoryError::ScopeNotReadable(_))
    ));
    assert_eq!(
        store.refresh_model("model-9", &outsider, |_| String::new()),
        Ok(None)
    );
    assert!(store.answer_model(&id, &outsider).is_none());
    assert!(store.model(&id, &outsider).is_none());
    assert!(store.models(&outsider).is_empty());
    assert!(store.fact(&foreign, &outsider).is_some(), "c2 is Ada's too");
    assert!(
        store.fact(&private, &outsider).is_none(),
        "no accessor hands back a fact the asker may not read"
    );
    assert!(
        store.facts(&outsider).iter().all(|fact| fact.id != private),
        "nor in bulk"
    );
}

/// Every accessor that returns stored content is behind the asker's scopes.
#[test]
fn accessors_are_gated_by_the_askers_scopes() {
    let mut store = MemoryStore::new();
    let mine = keep(
        &mut store,
        fact(
            ada(),
            "Ada",
            "drinks",
            "tea",
            "ada drinks tea",
            on(2026, 6, 1),
        ),
    );
    let theirs = keep(
        &mut store,
        fact(
            Scope::User(UserId::new("mallory".into())),
            "Mallory",
            "drinks",
            "coffee",
            "mallory drinks coffee",
            on(2026, 6, 1),
        ),
    );
    let scopes = ada_scopes();
    assert!(store.fact(&mine, &scopes).is_some());
    assert!(store.fact(&theirs, &scopes).is_none());
    assert_eq!(store.facts(&scopes).len(), 1);
}

/// A refresh accounts for every piece of evidence that exists, not only the
/// ones the budget let through — so a model stops being stale after one
/// refresh even when there is far more evidence than a refresh can read.
#[test]
fn a_refresh_clears_staleness_beyond_the_budget() {
    let mut store = MemoryStore::new();
    store.set_now(now());
    let scopes = ada_scopes();
    for day in 1..=60u32 {
        keep(
            &mut store,
            Fact::new(
                FactKind::World,
                ada(),
                "Ada",
                "lives in",
                "Utrecht",
                on(2026, 5, day),
            )
            .quote("ada lives in utrecht, in a house with a long garden")
            .source("thread:many"),
        );
    }
    let id = store.define_model(ada(), "Where does Ada live?");
    let mut seen = 0;
    store
        .refresh_model(&id, &scopes, |recalled: &[Recalled]| {
            seen = recalled.len();
            "Ada lives in Utrecht.".to_owned()
        })
        .expect("the asker may read this model");
    assert!(seen > 0 && seen < 60, "the budget cut the evidence: {seen}");
    assert!(
        store.stale_models(&scopes).is_empty(),
        "the watermark covers the evidence the writer never saw"
    );
    assert!(!store.answer_model(&id, &scopes).expect("an answer").stale);
}

/// An id is the key everything else cites by, so a collision is refused, and
/// an auto-assigned id steps around one already in use.
#[test]
fn a_duplicate_fact_id_is_refused() {
    let mut store = MemoryStore::new();
    let scopes = ada_scopes();
    let named = || fact(ada(), "Ada", "runs", "at dawn", "ada runs", on(2026, 6, 1));
    let mut first = named();
    first.id = "fact-1".into();
    keep(&mut store, first);
    let mut second = named();
    second.id = "fact-1".into();
    assert_eq!(
        store.retain(second),
        Err(MemoryError::DuplicateId("fact-1".into())),
        "a caller-supplied id is refused the second time"
    );
    // The auto-id that would have collided steps around it.
    let auto = keep(&mut store, named());
    assert_ne!(auto, "fact-1");
    assert!(store.fact(&auto, &scopes).is_some());
}

/// "What happened in June?" is answered across the whole of June, not from
/// the crowded end of it, and never from May or July.
#[test]
fn a_june_question_is_spread_across_june() {
    let mut store = MemoryStore::new();
    let mut dates = vec![(6u32, 2u32), (6, 4), (6, 13), (6, 16)];
    dates.extend((24..=30).map(|day| (6, day)));
    dates.extend([(5, 20), (5, 31), (7, 1), (7, 9)]);
    for (month, day) in dates {
        keep(
            &mut store,
            fact(
                ada(),
                "team",
                "did",
                "a thing",
                "the retro ran long",
                on(2026, month, day),
            ),
        );
    }

    // Eleven tokens per rendered fact, so four fit in 48 and a fifth does not.
    let recall = store.recall(
        &ada_scopes(),
        &RecallQuery::new("What happened in June?", 48, now()),
    );
    assert!(recall.tokens_used <= 48);

    let dates: Vec<String> = recall.items.iter().map(|item| iso_date(item.at)).collect();
    assert_eq!(dates.len(), 4, "a budget of 48 buys four facts: {dates:?}");
    assert!(
        dates.iter().all(|date| date.starts_with("2026-06")),
        "nothing from May or July: {dates:?}"
    );
    let day: Vec<u32> = dates
        .iter()
        .map(|date| date[8..].parse().expect("a day"))
        .collect();
    assert!(day.iter().any(|day| *day <= 8), "early June: {day:?}");
    assert!(
        day.iter().any(|day| (9..=23).contains(day)),
        "middle June: {day:?}"
    );
    assert!(day.iter().any(|day| *day >= 24), "late June: {day:?}");
    assert!(
        recall
            .items
            .iter()
            .all(|item| item.strategies.contains(&Strategy::Temporal)),
        "every item came through the temporal strategy"
    );
}

/// The budget is a ceiling and the asker's scopes are a wall, for every
/// budget and whatever the corpus looks like.
#[test]
fn the_budget_holds_and_no_scope_leaks() {
    let mut store = MemoryStore::new();
    let embedding = vec![0.5, 0.5, 0.5, 0.5];
    let quote = "the ledger migration was owned by ada";
    for index in 0..6 {
        keep(
            &mut store,
            fact(
                ada(),
                "payments",
                "owns",
                "the ledger",
                quote,
                on(2026, 6, index + 1),
            )
            .embedding(embedding.clone()),
        );
    }
    // The same words, the same embedding, in a scope Ada cannot read.
    let mallory = Scope::User(UserId::new("mallory".into()));
    keep(
        &mut store,
        fact(
            mallory.clone(),
            "payments",
            "owns",
            "the ledger",
            quote,
            on(2026, 6, 10),
        )
        .embedding(embedding.clone()),
    );

    let scopes = ada_scopes();
    let query = RecallQuery::new("who owns the ledger", 200, now()).embedding(embedding);
    for budget in [0, 1, 4, 9, 20, 200] {
        let recall = store.recall(&scopes, &RecallQuery::new(&query.text, budget, now()));
        assert!(recall.tokens_used <= budget, "budget {budget} was exceeded");
        for item in &recall.items {
            let scope = store.fact(&item.fact_id, &scopes).expect("in scope");
            assert!(
                scopes.contains(&scope.scope),
                "a fact from {} leaked",
                scope.scope
            );
        }
    }

    // And the matching out-of-scope fact is found the moment it is in scope.
    let theirs = scopes_for(
        &UserId::new("mallory".into()),
        Location::Dm,
        &ChannelMemberships::new(),
    );
    let foreign = store.recall(&theirs, &query);
    assert!(
        foreign.items.iter().any(|item| store
            .fact(&item.fact_id, &theirs)
            .is_some_and(|fact| fact.scope == mallory)),
        "a fact is invisible until its own scope can read it"
    );
}

/// A secret that reached an ingest path is a secret that is not in memory —
/// in any of the five text fields, and in neither the store nor a recall.
#[test]
fn every_text_field_is_redacted_on_the_way_in() {
    let mut store = MemoryStore::new();
    // Assembled at runtime so that no token-shaped literal ever reaches the
    // repository; the value still matches the GitHub-token redaction pattern
    // (`gh[pousr]_` followed by 36 token characters).
    let secret = ["gh", "p_", &"x1".repeat(18)].concat();
    let mail = "ada@example.com";
    let id = keep(
        &mut store,
        Fact::new(
            FactKind::Experience,
            shared(),
            "ada",
            &format!("used key {secret}"),
            "the deploy key",
            on(2026, 6, 2),
        )
        .quote(&format!("rotate {secret} and mail {mail} before friday"))
        .source(&format!("thread:deploy/{mail}")),
    );

    let scopes = ada_scopes();
    let stored = store.fact(&id, &scopes).expect("a stored fact");
    let fields = [
        &stored.quote,
        &stored.subject,
        &stored.predicate,
        &stored.object,
        &stored.source,
    ];
    for field in fields {
        assert!(!field.contains(&secret), "a secret survived in {field}");
        assert!(!field.contains(mail), "an email survived in {field}");
    }
    assert!(stored.quote.contains("rotate"));

    let recall = store.recall(
        &scopes,
        &RecallQuery::new("rotate the deploy key", 200, now()),
    );
    assert!(!recall.items.is_empty());
    for item in &recall.items {
        let rendered = item.render();
        assert!(
            !rendered.contains(&secret),
            "a secret in a recall: {rendered}"
        );
        assert!(!rendered.contains(mail), "an email in a recall: {rendered}");
    }
}

/// Facts are stored in the language they were written in, and found in it.
#[test]
fn a_dutch_fact_and_a_non_latin_entity_survive_a_round_trip() {
    let mut store = MemoryStore::new();
    let scopes = ada_scopes();
    let dutch = "We hebben de release vanmiddag uitgesteld naar volgende week";
    let dutch_id = keep(
        &mut store,
        Fact::new(
            FactKind::Experience,
            ada(),
            "het release-team",
            "heeft uitgesteld",
            "de release",
            on(2026, 6, 3),
        )
        .quote(dutch)
        .source("thread:nl-1"),
    );
    let tokyo_id = keep(
        &mut store,
        Fact::new(
            FactKind::World,
            ada(),
            "東京",
            "skyscraper",
            "173 m",
            on(2026, 6, 4),
        )
        .quote("we finally saw 東京 from the shinkansen")
        .source("thread:jp-1"),
    );

    assert_eq!(store.fact(&dutch_id, &scopes).expect("stored").quote, dutch);
    assert!(
        store
            .fact(&tokyo_id, &scopes)
            .expect("stored")
            .quote
            .contains("東京")
    );
    let dutch_hits = store.recall(&scopes, &RecallQuery::new("release uitgesteld", 200, now()));
    assert!(
        dutch_hits.items.iter().any(|item| item.fact_id == dutch_id),
        "Dutch words are keywords like any other"
    );
    let japanese_hits = store.recall(&scopes, &RecallQuery::new("東京", 200, now()));
    assert!(
        japanese_hits
            .items
            .iter()
            .any(|item| item.fact_id == tokyo_id),
        "a non-Latin script is not stripped on the way in"
    );
    assert!(questions_match("東京", "東京"));
}

/// Reflection settles on the best tier it can: the stored answer, then the
/// consolidated belief, then the raw claim.
#[test]
fn reflection_prefers_a_model_then_an_observation_then_a_fact() {
    let mut store = MemoryStore::new();
    store.set_now(now());
    let scopes = ada_scopes();
    keep(
        &mut store,
        fact(
            ada(),
            "Ada",
            "drinks",
            "oat flat whites",
            "ada only drinks oat flat whites",
            on(2026, 2, 1),
        ),
    );
    store.consolidate();

    // Tier 3 first: a claim whose words match nothing else, so neither the
    // keyword nor the graph strategy drags in the consolidated fact above,
    // and which no observation covers because it arrives after the last
    // consolidate.
    keep(
        &mut store,
        fact(
            ada(),
            "the sunrise",
            "came",
            "early",
            "the sunrise came early today",
            on(2026, 6, 1),
        ),
    );
    assert_eq!(
        store
            .recall(&scopes, &RecallQuery::new("sunrise", 200, now()))
            .items
            .len(),
        1,
        "one claim, one hit"
    );
    match store.reflect(&scopes, &RecallQuery::new("sunrise", 200, now())) {
        Reflection::Facts { citations } => assert_eq!(citations.len(), 1),
        other => panic!("expected raw facts for an unconsolidated claim: {other:?}"),
    }

    // Tier 2: the same claim, once it has been consolidated.
    store.consolidate();
    match store.reflect(&scopes, &RecallQuery::new("ada drinks", 200, now())) {
        Reflection::Observations {
            observations,
            citations,
        } => {
            assert_eq!(observations.len(), 1);
            assert_eq!(observations[0].belief, "oat flat whites");
            assert_eq!(observations[0].proof_count, 1);
            assert!(!citations.is_empty());
        }
        other => panic!("expected an observation: {other:?}"),
    }

    // Tier 1: a question the brain has already answered — but only if the
    // answer fits the query's budget.
    let id = store.define_model(ada(), "What does Ada drink?");
    store
        .refresh_model(&id, &scopes, |recalled: &[Recalled]| {
            assert!(!recalled.is_empty(), "the writer sees the evidence");
            "Oat flat whites, and nothing else.".to_owned()
        })
        .expect("the asker may read this model");
    match store.reflect(
        &scopes,
        &RecallQuery::new("what does ada drink", 200, now()),
    ) {
        Reflection::Model(answer) => {
            assert_eq!(answer.text, "Oat flat whites, and nothing else.");
            assert!(answer.tokens_used > 0);
            assert!(!answer.stale);
            assert_eq!(answer.citations.len(), 1);
        }
        other => panic!("expected the stored model answer: {other:?}"),
    }
    assert!(
        !matches!(
            store.reflect(&scopes, &RecallQuery::new("what does ada drink", 1, now())),
            Reflection::Model(_)
        ),
        "an answer that does not fit the budget falls through"
    );
    assert_eq!(store.observations(&scopes).len(), 2);
}

proptest! {
    /// Whatever the budget, the result costs no more than it and stays inside
    /// the asker's scopes.
    #[test]
    fn the_budget_is_never_exceeded(
        budget in 0usize..128,
        corpus in prop::collection::vec((0u8..3, 0u32..30, 0u32..99), 0..12),
    ) {
        let mut store = MemoryStore::new();
        for (kind, day, length) in corpus {
            let scope = match kind {
                0 => shared(),
                1 => ada(),
                _ => channel("c1"),
            };
            keep(&mut store, fact(
                scope,
                "ada",
                "wrote",
                "a note",
                &"a note about the ledger ".repeat(length as usize),
                on(2026, 6, day + 1),
            ));
        }
        let scopes = ada_scopes();
        let recall = store.recall(&scopes, &RecallQuery::new("ada wrote a note", budget, now()));
        prop_assert!(recall.tokens_used <= budget);
        for item in &recall.items {
            let fact = store.fact(&item.fact_id, &scopes).expect("in scope");
            prop_assert!(scopes.contains(&fact.scope), "a fact from {} leaked", fact.scope);
        }
    }
}
