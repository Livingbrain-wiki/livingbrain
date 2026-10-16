//! Temporal facts (docs/temporal-facts.md): partial dates, supersession,
//! disputes, cardinality, as-of reads and the two renderings.

use livingbrain_memory::temporal::{DAY, days_from_civil};
use livingbrain_memory::timeline::{
    Cardinality, FactStatus, PartialDate, Precision, TimedFact, as_of_in_question, corroborated,
    normalize_attribute, render_line, render_markdown, resolve, valid_at,
};

fn at(year: i64, month: u32, day: u32) -> i64 {
    days_from_civil(year, month, day) * DAY
}

fn fact(id: &str, value: &str, from: &str, observed: i64) -> TimedFact {
    TimedFact {
        id: id.to_owned(),
        subject: "person/nick".to_owned(),
        attribute: "shoe_size".to_owned(),
        value: value.to_owned(),
        unit: Some("EU".to_owned()),
        valid_from: PartialDate::parse(from).expect("a date"),
        stated_valid_to: None,
        valid_to: None,
        observed_at: observed,
        last_confirmed_at: observed,
        status: FactStatus::Current,
        supersedes: None,
        superseded_by: None,
        source: Some(format!("note:{id}")),
        quote: None,
        confidence: 1.0,
        cardinality: Cardinality::One,
    }
}

fn by_id<'a>(timeline: &'a [TimedFact], id: &str) -> &'a TimedFact {
    timeline.iter().find(|f| f.id == id).expect("the fact")
}

#[test]
fn partial_dates_keep_their_precision() {
    let year = PartialDate::parse("2026").unwrap();
    assert_eq!(year.precision(), Precision::Year);
    assert_eq!(year.start(), at(2026, 1, 1));
    assert_eq!(year.end(), at(2027, 1, 1));
    assert_eq!(year.as_of_instant(), at(2027, 1, 1) - 1);

    let month = PartialDate::parse("2026-12").unwrap();
    assert_eq!(month.precision(), Precision::Month);
    assert_eq!(month.end(), at(2027, 1, 1));

    let day = PartialDate::parse("2024-02-29").unwrap();
    assert_eq!(day.precision(), Precision::Day);
    assert_eq!(day.end(), at(2024, 3, 1));

    let instant = PartialDate::parse("2026-10-15T11:12:00+02:00").unwrap();
    assert_eq!(instant.precision(), Precision::Second);
    assert_eq!(instant.text(), "2026-10-15T09:12:00Z");
    assert_eq!(instant.start(), at(2026, 10, 15) + 9 * 3600 + 12 * 60);
    assert!(PartialDate::parse("2026-10-15T09:12:00.250Z").is_ok());

    for bad in [
        "",
        "26",
        "2026-13",
        "2025-02-29",
        "2026-1",
        "2026-10-15T25:00:00Z",
        "2026T10:00:00Z",
        "next year",
        "0999",
    ] {
        assert!(PartialDate::parse(bad).is_err(), "{bad} should not parse");
    }
}

#[test]
fn attributes_normalise() {
    assert_eq!(
        normalize_attribute("Shoe Size").as_deref(),
        Some("shoe_size")
    );
    assert_eq!(
        normalize_attribute("shoe-size").as_deref(),
        Some("shoe_size")
    );
    assert_eq!(
        normalize_attribute("  uses__tool ").as_deref(),
        Some("uses_tool")
    );
    assert_eq!(normalize_attribute("!!!"), None);
    assert_eq!(normalize_attribute(&"a".repeat(65)), None);
    assert_eq!(Cardinality::default_for("uses_tool"), Cardinality::Many);
    assert_eq!(Cardinality::default_for("shoe_size"), Cardinality::One);
}

#[test]
fn a_new_value_supersedes_and_history_is_kept() {
    let mut timeline = vec![
        fact("a", "40", "2025", at(2025, 3, 2)),
        fact("b", "41", "2026", at(2026, 10, 15)),
    ];
    resolve(&mut timeline);
    let a = by_id(&timeline, "a");
    assert_eq!(a.status, FactStatus::Superseded);
    assert_eq!(a.valid_to.as_ref().map(PartialDate::text), Some("2026"));
    assert_eq!(a.superseded_by.as_deref(), Some("b"));
    let b = by_id(&timeline, "b");
    assert_eq!(b.status, FactStatus::Current);
    assert_eq!(b.valid_to, None);
    assert_eq!(b.supersedes.as_deref(), Some("a"));

    // The order facts arrive in does not change the resolution.
    let mut reversed = vec![timeline[1].clone(), timeline[0].clone()];
    resolve(&mut reversed);
    assert_eq!(by_id(&reversed, "a"), by_id(&timeline, "a"));

    // Now: 41, was 40. As of 2025: 40, later 41.
    let now = at(2026, 10, 15);
    let cite = |f: &TimedFact| Some(format!("F-{}", f.id));
    assert_eq!(
        render_line(&timeline, now, &cite).unwrap(),
        "shoe_size(person/nick) = 41 EU [valid 2026..now, observed 2026-10-15, source F-b] \
         (was 40 EU 2025..2026)"
    );
    let as_of_2025 = PartialDate::parse("2025").unwrap().as_of_instant();
    assert_eq!(valid_at(&timeline, as_of_2025)[0].value, "40");
    assert_eq!(
        render_line(&timeline, as_of_2025, &cite).unwrap(),
        "shoe_size(person/nick) = 40 EU [valid 2025..2026, observed 2025-03-02, source F-a] \
         (later 41 EU 2026..now)"
    );
}

#[test]
fn a_backfilled_older_value_slots_in_before() {
    let mut timeline = vec![
        fact("a", "40", "2025", at(2025, 3, 2)),
        fact("b", "41", "2026", at(2026, 10, 15)),
        fact("c", "39", "2024", at(2026, 10, 16)),
    ];
    resolve(&mut timeline);
    let c = by_id(&timeline, "c");
    assert_eq!(c.status, FactStatus::Superseded);
    assert_eq!(c.range(), "2024..2025");
    assert_eq!(c.superseded_by.as_deref(), Some("a"));
    assert_eq!(by_id(&timeline, "b").status, FactStatus::Current);
    assert_eq!(by_id(&timeline, "a").supersedes.as_deref(), Some("c"));
}

#[test]
fn same_start_different_values_is_disputed() {
    let mut timeline = vec![
        fact("a", "40", "2026", at(2026, 1, 2)),
        fact("b", "42", "2026", at(2026, 1, 3)),
    ];
    resolve(&mut timeline);
    assert!(timeline.iter().all(|f| f.status == FactStatus::Disputed));
    let line = render_line(&timeline, at(2026, 6, 1), &|_| None).unwrap();
    assert!(line.contains("{disputed: "), "{line}");
    assert!(line.contains("42 EU") && line.contains("40 EU"), "{line}");
    // Retracting one settles it.
    timeline[1].status = FactStatus::Retracted;
    resolve(&mut timeline);
    assert_eq!(by_id(&timeline, "a").status, FactStatus::Current);
}

#[test]
fn a_newer_observation_that_the_old_value_still_held_is_disputed() {
    let mut timeline = vec![
        fact("new", "41", "2026", at(2026, 2, 1)),
        fact("old", "40", "2025", at(2026, 3, 1)),
    ];
    timeline[1].stated_valid_to = Some(PartialDate::parse("2027").unwrap());
    resolve(&mut timeline);
    assert_eq!(by_id(&timeline, "old").status, FactStatus::Disputed);
    assert_eq!(by_id(&timeline, "new").status, FactStatus::Disputed);

    // The same stated end, observed *before* the new value, is just history.
    let mut ordered = vec![
        fact("old", "40", "2025", at(2025, 3, 1)),
        fact("new", "41", "2026", at(2026, 2, 1)),
    ];
    ordered[0].stated_valid_to = Some(PartialDate::parse("2027").unwrap());
    resolve(&mut ordered);
    assert_eq!(by_id(&ordered, "old").status, FactStatus::Superseded);
    assert_eq!(by_id(&ordered, "old").range(), "2025..2026");
}

#[test]
fn many_valued_attributes_accumulate() {
    let mut a = fact("a", "ripgrep", "2025", at(2025, 1, 1));
    let mut b = fact("b", "jj", "2026", at(2026, 1, 1));
    let mut c = fact("c", "svn", "2010", at(2025, 1, 1));
    for f in [&mut a, &mut b, &mut c] {
        f.attribute = "uses_tool".to_owned();
        f.unit = None;
        f.cardinality = Cardinality::Many;
    }
    c.stated_valid_to = Some(PartialDate::parse("2015").unwrap());
    let mut timeline = vec![a, b, c];
    resolve(&mut timeline);
    assert!(timeline.iter().all(|f| f.status == FactStatus::Current));
    let now = at(2026, 6, 1);
    let valid: Vec<&str> = valid_at(&timeline, now)
        .iter()
        .map(|f| f.value.as_str())
        .collect();
    assert_eq!(valid.len(), 2);
    assert!(valid.contains(&"ripgrep") && valid.contains(&"jj"));
    let line = render_line(&timeline, now, &|_| None).unwrap();
    assert!(line.starts_with("uses_tool(person/nick) = {"), "{line}");
    assert!(line.contains("(was svn 2010..2015)"), "{line}");
}

#[test]
fn a_repeat_is_a_corroboration() {
    let mut timeline = vec![fact("a", "40", "2025", at(2025, 3, 2))];
    resolve(&mut timeline);
    let again = PartialDate::parse("2025-06").unwrap();
    assert_eq!(
        corroborated(&timeline, "40", Some("EU"), &again).map(|f| f.id.as_str()),
        Some("a")
    );
    assert!(corroborated(&timeline, "40", Some("US"), &again).is_none());
    assert!(corroborated(&timeline, "41", Some("EU"), &again).is_none());
}

#[test]
fn a_question_names_its_time() {
    let now = at(2026, 10, 15);
    assert_eq!(
        as_of_in_question("what was my shoe size in 2025?", now),
        Some(at(2026, 1, 1) - 1)
    );
    assert_eq!(
        as_of_in_question("shoe size as of 2025-06", now),
        Some(at(2025, 7, 1) - 1)
    );
    assert_eq!(
        as_of_in_question("what did we use in June 2024", now),
        Some(at(2024, 7, 1) - 1)
    );
    assert_eq!(as_of_in_question("what's my shoe size", now), None);
    // A future year is not a time the brain can answer about.
    assert_eq!(as_of_in_question("plans in 2030", now), None);
}

#[test]
fn the_page_section_is_deterministic() {
    let mut timeline = vec![
        fact("a", "40", "2025", at(2025, 3, 2)),
        fact("b", "41", "2026", at(2026, 10, 15)),
    ];
    resolve(&mut timeline);
    let markdown = render_markdown(&[timeline.clone()], at(2026, 10, 15));
    assert_eq!(
        markdown,
        "## Facts\n\n### Current\n\n\
         | Attribute | Value | Valid | Observed | Source |\n\
         | :--- | :--- | :--- | :--- | :--- |\n\
         | shoe_size | 41 EU | 2026 – now | 2026-10-15 | note:b |\n\
         \n### History\n\n\
         - **2026 – now** · shoe_size = 41 EU · observed 2026-10-15 · current\n\
         - **2025 – 2026** · shoe_size = 40 EU · observed 2025-03-02 · superseded by b\n"
    );
    timeline.reverse();
    assert_eq!(render_markdown(&[timeline], at(2026, 10, 15)), markdown);
    assert_eq!(render_markdown(&[], 0), "");
}
