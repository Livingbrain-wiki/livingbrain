//! Temporal facts: what was true **when**, and what the brain knew **when**.
//!
//! The format is `docs/temporal-facts.md` (v1). This module is its pure
//! half: partial dates, the attribute's cardinality, the deterministic
//! resolution of one subject+attribute timeline (supersession, disputes),
//! and the two renderings — the Markdown a page shows and the one line a
//! model reads. Storage is the pages module's; nothing here does I/O.
//!
//! The model is bi-temporal: [`TimedFact::valid_from`]/[`TimedFact::valid_to`]
//! say when a claim is true in the world, [`TimedFact::observed_at`] says
//! when the brain learned it, and neither is ever rewritten to mean the
//! other. New information never overwrites: [`resolve`] closes the old
//! fact's validity where the new one starts and keeps it as history.

use crate::temporal::{DAY, civil_from_days, days_from_civil, iso_date};

/// How precisely a [`PartialDate`] was written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Precision {
    /// `2026`
    Year,
    /// `2026-10`
    Month,
    /// `2026-10-15`
    Day,
    /// `2026-10-15T09:12:00Z`
    Second,
}

impl Precision {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
            Self::Second => "second",
        }
    }
}

/// A date as precise as the writer knew it: `2026`, `2026-10`,
/// `2026-10-15` or an RFC 3339 instant.
///
/// [`text`](Self::text) is what was written (normalised to UTC for an
/// instant) and is what is stored and shown; [`start`](Self::start) is the
/// first second of the period, which is what sorts and compares.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PartialDate {
    text: String,
    precision: Precision,
    start: i64,
}

/// Why a date did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateError(pub String);

impl std::fmt::Display for DateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is not a date: use YYYY, YYYY-MM, YYYY-MM-DD or an RFC 3339 instant",
            self.0
        )
    }
}

impl std::error::Error for DateError {}

fn digits(text: &str, len: usize) -> Option<u32> {
    (text.len() == len && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let next = if month == 12 {
        days_from_civil(year + 1, 1, 1)
    } else {
        days_from_civil(year, month + 1, 1)
    };
    u32::try_from(next - days_from_civil(year, month, 1)).unwrap_or(31)
}

impl PartialDate {
    /// Parses `YYYY`, `YYYY-MM`, `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM:SS`
    /// with an optional fraction and a `Z` or `±HH:MM` offset. Years are
    /// 1000..=9999.
    ///
    /// # Errors
    ///
    /// [`DateError`] for anything else, including a day the month does not
    /// have.
    pub fn parse(raw: &str) -> Result<Self, DateError> {
        let bad = || DateError(raw.to_owned());
        let raw_trim = raw.trim();
        let (date, time) = match raw_trim.split_once(['T', 't']) {
            Some((date, time)) => (date, Some(time)),
            None => (raw_trim, None),
        };
        let mut parts = date.split('-');
        let year = digits(parts.next().ok_or_else(bad)?, 4).ok_or_else(bad)?;
        if !(1000..=9999).contains(&year) {
            return Err(bad());
        }
        let year = i64::from(year);
        let month = parts.next().map(|m| digits(m, 2).ok_or_else(bad));
        let day = parts.next().map(|d| digits(d, 2).ok_or_else(bad));
        if parts.next().is_some() {
            return Err(bad());
        }
        let Some(month) = month.transpose()? else {
            if time.is_some() {
                return Err(bad());
            }
            return Ok(Self {
                text: format!("{year:04}"),
                precision: Precision::Year,
                start: days_from_civil(year, 1, 1) * DAY,
            });
        };
        if !(1..=12).contains(&month) {
            return Err(bad());
        }
        let Some(day) = day.transpose()? else {
            if time.is_some() {
                return Err(bad());
            }
            return Ok(Self {
                text: format!("{year:04}-{month:02}"),
                precision: Precision::Month,
                start: days_from_civil(year, month, 1) * DAY,
            });
        };
        if day == 0 || day > days_in_month(year, month) {
            return Err(bad());
        }
        let midnight = days_from_civil(year, month, day) * DAY;
        let Some(time) = time else {
            return Ok(Self {
                text: format!("{year:04}-{month:02}-{day:02}"),
                precision: Precision::Day,
                start: midnight,
            });
        };
        let (clock, offset) = if let Some(clock) = time.strip_suffix(['Z', 'z']) {
            (clock, 0)
        } else {
            let at = time.rfind(['+', '-']).ok_or_else(bad)?;
            let (clock, zone) = time.split_at(at);
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (hours, minutes) = zone[1..].split_once(':').ok_or_else(bad)?;
            let hours = digits(hours, 2).ok_or_else(bad)?;
            let minutes = digits(minutes, 2).ok_or_else(bad)?;
            if hours > 23 || minutes > 59 {
                return Err(bad());
            }
            (clock, sign * i64::from(hours * 3600 + minutes * 60))
        };
        let clock = clock.split_once('.').map_or(clock, |(whole, fraction)| {
            if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
                ""
            } else {
                whole
            }
        });
        let mut hms = clock.split(':');
        let hour = digits(hms.next().ok_or_else(bad)?, 2).ok_or_else(bad)?;
        let minute = digits(hms.next().ok_or_else(bad)?, 2).ok_or_else(bad)?;
        let second = digits(hms.next().ok_or_else(bad)?, 2).ok_or_else(bad)?;
        if hms.next().is_some() || hour > 23 || minute > 59 || second > 60 {
            return Err(bad());
        }
        let at = midnight + i64::from(hour * 3600 + minute * 60 + second.min(59)) - offset;
        Ok(Self::instant(at))
    }

    /// An instant, at second precision, written as RFC 3339 in UTC.
    #[must_use]
    pub fn instant(at: i64) -> Self {
        Self {
            text: rfc3339(at),
            precision: Precision::Second,
            start: at,
        }
    }

    /// The date as written (an instant normalised to UTC `…Z`).
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How precisely it was written.
    #[must_use]
    pub fn precision(&self) -> Precision {
        self.precision
    }

    /// The first second of the period, unix seconds.
    #[must_use]
    pub fn start(&self) -> i64 {
        self.start
    }

    /// The first second **after** the period: the start of the next year,
    /// month or day, or one second after an instant.
    #[must_use]
    pub fn end(&self) -> i64 {
        let (year, month, day) = civil_from_days(self.start.div_euclid(DAY));
        match self.precision {
            Precision::Year => days_from_civil(year + 1, 1, 1) * DAY,
            Precision::Month if month == 12 => days_from_civil(year + 1, 1, 1) * DAY,
            Precision::Month => days_from_civil(year, month + 1, 1) * DAY,
            Precision::Day => days_from_civil(year, month, day) * DAY + DAY,
            Precision::Second => self.start + 1,
        }
    }

    /// The instant an `as_of` of this date asks about: the **last** second
    /// of the period, so `as_of=2025` is "as of the end of 2025".
    #[must_use]
    pub fn as_of_instant(&self) -> i64 {
        self.end() - 1
    }
}

impl std::fmt::Display for PartialDate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// `at` as `YYYY-MM-DDTHH:MM:SSZ`.
#[must_use]
pub fn rfc3339(at: i64) -> String {
    let secs = at.rem_euclid(DAY);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        iso_date(at),
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Whether an attribute holds one value at a time or several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cardinality {
    /// One value at a time: a new value ends the old one (`shoe_size`).
    One,
    /// Values accumulate; each has its own timeline (`uses_tool`).
    Many,
}

/// The built-in registry's `many` attributes; everything else is `one`.
const MANY: &[&str] = &[
    "alias",
    "aliases",
    "contributor",
    "contributors",
    "depends_on",
    "dislikes",
    "knows",
    "likes",
    "member_of",
    "owns",
    "reports",
    "skill",
    "skills",
    "speaks",
    "tag",
    "tags",
    "uses",
    "uses_tool",
    "works_on",
];

impl Cardinality {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::One => "one",
            Self::Many => "many",
        }
    }

    /// Parses a wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "one" => Some(Self::One),
            "many" => Some(Self::Many),
            _ => None,
        }
    }

    /// The built-in registry's cardinality for a normalised attribute.
    #[must_use]
    pub fn default_for(attribute: &str) -> Self {
        if MANY.contains(&attribute) {
            Self::Many
        } else {
            Self::One
        }
    }
}

/// The longest attribute key.
pub const MAX_ATTRIBUTE_LEN: usize = 64;

/// An attribute in its normal form — lowercase `snake_case`,
/// `[a-z0-9_]`, 1..=64 chars — or `None` when nothing is left. Spaces,
/// `-`, `.` and `/` become `_`; runs of `_` collapse; anything else
/// non-alphanumeric is dropped.
#[must_use]
pub fn normalize_attribute(raw: &str) -> Option<String> {
    let mut out = String::new();
    for c in raw.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if matches!(c, ' ' | '-' | '_' | '.' | '/') && !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_owned();
    (!out.is_empty() && out.len() <= MAX_ATTRIBUTE_LEN).then_some(out)
}

/// A fact's standing as a belief (its validity range is separate).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FactStatus {
    /// Nobody has replaced it.
    Current,
    /// A later fact replaced it; kept as history.
    Superseded,
    /// Somebody said it was never true.
    Retracted,
    /// It conflicts with another fact and nothing orders them.
    Disputed,
}

impl FactStatus {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Superseded => "superseded",
            Self::Retracted => "retracted",
            Self::Disputed => "disputed",
        }
    }

    /// Parses a wire name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "current" => Some(Self::Current),
            "superseded" => Some(Self::Superseded),
            "retracted" => Some(Self::Retracted),
            "disputed" => Some(Self::Disputed),
            _ => None,
        }
    }
}

/// One temporal fact (format v1).
#[derive(Debug, Clone, PartialEq)]
pub struct TimedFact {
    /// `fact-<ULID>`.
    pub id: String,
    /// `<entity_type>/<slug>`.
    pub subject: String,
    /// Normalised attribute key.
    pub attribute: String,
    pub value: String,
    pub unit: Option<String>,
    /// When it became true in the world.
    pub valid_from: PartialDate,
    /// The `valid_to` the writer stated, if any — kept so a re-resolution
    /// can recompute the effective one.
    pub stated_valid_to: Option<PartialDate>,
    /// The effective end, exclusive: the stated one, or where a successor
    /// begins. `None` is open-ended.
    pub valid_to: Option<PartialDate>,
    /// When the brain learned it, unix seconds.
    pub observed_at: i64,
    /// When a write last said this again (the same value, for the same
    /// timeline), unix seconds: set on insert, moved forward by a repeat
    /// instead of a duplicate row. What "stale" is measured from.
    pub last_confirmed_at: i64,
    pub status: FactStatus,
    pub supersedes: Option<String>,
    pub superseded_by: Option<String>,
    pub source: Option<String>,
    pub quote: Option<String>,
    /// 0..=1.
    pub confidence: f64,
    pub cardinality: Cardinality,
}

impl TimedFact {
    /// Whether the fact is true in the world at `at` (by its effective range).
    #[must_use]
    pub fn valid_at(&self, at: i64) -> bool {
        self.valid_from.start() <= at && self.valid_to.as_ref().is_none_or(|to| at < to.start())
    }

    /// Whether this fact says the same thing as `other`: value and unit.
    #[must_use]
    pub fn same_value(&self, value: &str, unit: Option<&str>) -> bool {
        self.value == value && self.unit.as_deref() == unit
    }

    /// `41 EU`, or `41` with no unit.
    #[must_use]
    pub fn display_value(&self) -> String {
        match &self.unit {
            Some(unit) if !unit.is_empty() => format!("{} {unit}", self.value),
            _ => self.value.clone(),
        }
    }

    /// `2025..2026` or `2026..now`.
    #[must_use]
    pub fn range(&self) -> String {
        format!(
            "{}..{}",
            self.valid_from,
            self.valid_to
                .as_ref()
                .map_or_else(|| "now".to_owned(), ToString::to_string)
        )
    }

    fn order_key(&self) -> (i64, i64, String) {
        (self.valid_from.start(), self.observed_at, self.id.clone())
    }
}

/// The live fact a new claim of `value`/`unit` from `valid_from` repeats, if
/// any: same value and unit, not retracted, and either valid at the new
/// fact's start or starting at the same point. A repeat is a corroboration,
/// not a new fact.
#[must_use]
pub fn corroborated<'a>(
    timeline: &'a [TimedFact],
    value: &str,
    unit: Option<&str>,
    valid_from: &PartialDate,
) -> Option<&'a TimedFact> {
    timeline.iter().find(|fact| {
        fact.status != FactStatus::Retracted
            && fact.same_value(value, unit)
            && (fact.valid_at(valid_from.start()) || fact.valid_from.start() == valid_from.start())
    })
}

/// Re-resolves one subject+attribute timeline in place: effective
/// `valid_to`, `status`, `supersedes` and `superseded_by` for every fact
/// that is not retracted. Deterministic: the same facts always resolve the
/// same way, whatever order they arrive in. The rules are §2 of
/// `docs/temporal-facts.md`.
pub fn resolve(timeline: &mut [TimedFact]) {
    for fact in timeline.iter_mut() {
        if fact.status == FactStatus::Retracted {
            continue;
        }
        fact.status = FactStatus::Current;
        fact.valid_to = fact.stated_valid_to.clone();
        fact.supersedes = None;
        fact.superseded_by = None;
    }
    let cardinality = timeline
        .first()
        .map_or(Cardinality::One, |fact| fact.cardinality);
    if cardinality == Cardinality::Many {
        // Each value is its own timeline; a new value ends nothing.
        return;
    }

    let mut live: Vec<usize> = (0..timeline.len())
        .filter(|&i| timeline[i].status != FactStatus::Retracted)
        .collect();
    live.sort_by_key(|&i| timeline[i].order_key());

    // Groups of equal `valid_from` start, in order.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for i in live {
        match groups.last_mut() {
            Some(group)
                if timeline[group[0]].valid_from.start() == timeline[i].valid_from.start() =>
            {
                group.push(i);
            }
            _ => groups.push(vec![i]),
        }
    }

    // Rule 4: same start, different values.
    for group in &groups {
        let first = &timeline[group[0]];
        let (value, unit) = (first.value.clone(), first.unit.clone());
        if group
            .iter()
            .any(|&i| !timeline[i].same_value(&value, unit.as_deref()))
        {
            for &i in group {
                timeline[i].status = FactStatus::Disputed;
            }
        }
    }

    // Rules 5 and 6: each group is ended by the next one.
    for pair in groups.windows(2) {
        let (this, next) = (&pair[0], &pair[1]);
        let next_from = timeline[next[0]].valid_from.clone();
        let next_newest = next
            .iter()
            .map(|&i| timeline[i].observed_at)
            .max()
            .unwrap_or(i64::MIN);
        let successor = next
            .iter()
            .copied()
            .max_by_key(|&i| (timeline[i].observed_at, timeline[i].id.clone()))
            .map(|i| timeline[i].id.clone());
        let predecessor = this
            .iter()
            .copied()
            .max_by_key(|&i| (timeline[i].observed_at, timeline[i].id.clone()))
            .map(|i| timeline[i].id.clone());
        let mut conflict = false;
        for &i in this {
            let fact = &mut timeline[i];
            match &fact.stated_valid_to {
                Some(to) if to.start() > next_from.start() && fact.observed_at > next_newest => {
                    // A newer observation says the old value still held.
                    conflict = true;
                    fact.status = FactStatus::Disputed;
                }
                Some(to) if to.start() <= next_from.start() => {
                    fact.valid_to = Some(to.clone());
                }
                _ => fact.valid_to = Some(next_from.clone()),
            }
            if fact.status != FactStatus::Disputed {
                fact.status = FactStatus::Superseded;
            }
            if !conflict {
                fact.superseded_by.clone_from(&successor);
            }
        }
        for &i in next {
            if conflict {
                timeline[i].status = FactStatus::Disputed;
            } else {
                timeline[i].supersedes.clone_from(&predecessor);
            }
        }
    }
}

/// The facts of one timeline valid at `at`, newest observation first;
/// retracted ones never count.
#[must_use]
pub fn valid_at(timeline: &[TimedFact], at: i64) -> Vec<&TimedFact> {
    let mut found: Vec<&TimedFact> = timeline
        .iter()
        .filter(|fact| fact.status != FactStatus::Retracted && fact.valid_at(at))
        .collect();
    found.sort_by(|a, b| {
        b.observed_at
            .cmp(&a.observed_at)
            .then_with(|| a.value.cmp(&b.value))
    });
    found
}

/// The timeline in history order: newest `valid_from` first, then newest
/// observation.
#[must_use]
pub fn history(timeline: &[TimedFact]) -> Vec<&TimedFact> {
    let mut all: Vec<&TimedFact> = timeline.iter().collect();
    all.sort_by_key(|fact| std::cmp::Reverse(fact.order_key()));
    all
}

/// The one line a model reads for a subject+attribute, as of `at`:
///
/// `shoe_size(person/nick) = 41 EU [valid 2026..now, observed 2026-10-15, source F1] (was 40 EU 2025..2026)`
///
/// `cite` names a fact's source in the brief (e.g. `F1`), or `None` for a
/// fact with no source. Returns `None` for a timeline with nothing in it
/// that is not retracted.
#[must_use]
pub fn render_line(
    timeline: &[TimedFact],
    at: i64,
    cite: &dyn Fn(&TimedFact) -> Option<String>,
) -> Option<String> {
    let first = timeline.first()?;
    let head = format!("{}({})", first.attribute, first.subject);
    let valid = valid_at(timeline, at);
    let bracket = |fact: &TimedFact| {
        let mut parts = vec![
            format!("valid {}", fact.range()),
            format!("observed {}", iso_date(fact.observed_at)),
        ];
        if let Some(source) = cite(fact) {
            parts.push(format!("source {source}"));
        }
        if fact.status == FactStatus::Disputed {
            parts.push("disputed".to_owned());
        }
        format!("[{}]", parts.join(", "))
    };
    let live: Vec<&TimedFact> = timeline
        .iter()
        .filter(|fact| fact.status != FactStatus::Retracted)
        .collect();
    if live.is_empty() {
        return None;
    }
    let mut line = match (first.cardinality, valid.as_slice()) {
        (_, []) => format!("{head} = (no value valid at {})", iso_date(at)),
        (Cardinality::One, [only]) => {
            format!("{head} = {} {}", only.display_value(), bracket(only))
        }
        (Cardinality::One, several) => format!(
            "{head} = {{disputed: {}}}",
            several
                .iter()
                .map(|fact| format!("{} {}", fact.display_value(), bracket(fact)))
                .collect::<Vec<_>>()
                .join(" | ")
        ),
        (Cardinality::Many, several) => format!(
            "{head} = {{{}}}",
            several
                .iter()
                .map(|fact| format!("{} {}", fact.display_value(), bracket(fact)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    let was: Vec<String> = history(timeline)
        .into_iter()
        .filter(|fact| {
            fact.status != FactStatus::Retracted
                && fact.valid_from.start() <= at
                && !fact.valid_at(at)
        })
        .map(|fact| format!("{} {}", fact.display_value(), fact.range()))
        .collect();
    if !was.is_empty() {
        line.push_str(&format!(" (was {})", was.join("; ")));
    }
    let mut later: Vec<&TimedFact> = live
        .into_iter()
        .filter(|fact| fact.valid_from.start() > at)
        .collect();
    later.sort_by_key(|fact| fact.order_key());
    if !later.is_empty() {
        line.push_str(&format!(
            " (later {})",
            later
                .iter()
                .map(|fact| format!("{} {}", fact.display_value(), fact.range()))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Some(line)
}

/// How a model should read the fact lines: put at the top of a brief.
pub const PROMPT_CONVENTION: &str = "Facts below are temporal. Each line is \
`attribute(subject) = value [valid FROM..TO, observed DATE, source]`, where TO is `now` \
when the fact still holds. Prefer the value valid now (or at the time the question asks \
about). When a value changed and the change is relevant, say so (\"41, was 40 until \
2026\"). Treat `disputed` as unsettled and say so. Cite the source.";

/// The deterministic `## Facts` section of an entity page: a Current table
/// (facts valid at `at`, by attribute) and a History list (newest first).
/// `timelines` are one subject's timelines, each one subject+attribute.
/// Empty when there is nothing to show.
#[must_use]
pub fn render_markdown(timelines: &[Vec<TimedFact>], at: i64) -> String {
    // One subject may have the same attribute in two scopes (personal and
    // shared): both are shown, in a stable order.
    let mut by_attribute: Vec<&Vec<TimedFact>> =
        timelines.iter().filter(|t| !t.is_empty()).collect();
    by_attribute.sort_by(|a, b| a[0].attribute.cmp(&b[0].attribute));
    if by_attribute.is_empty() {
        return String::new();
    }
    let cell = |text: &str| text.replace('|', "\\|").replace('\n', " ");
    let mut out = String::from("## Facts\n\n### Current\n\n");
    let mut rows = Vec::new();
    for timeline in &by_attribute {
        for fact in valid_at(timeline, at) {
            rows.push(format!(
                "| {} | {} | {} | {} | {} |",
                cell(&fact.attribute),
                cell(&fact.display_value()),
                fact.range().replace("..", " – "),
                iso_date(fact.observed_at),
                cell(fact.source.as_deref().unwrap_or("")),
            ));
        }
    }
    if rows.is_empty() {
        out.push_str("Nothing is valid now.\n");
    } else {
        out.push_str("| Attribute | Value | Valid | Observed | Source |\n");
        out.push_str("| :--- | :--- | :--- | :--- | :--- |\n");
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
    }
    out.push_str("\n### History\n\n");
    let mut all: Vec<&TimedFact> = by_attribute.iter().flat_map(|t| t.iter()).collect();
    all.sort_by(|a, b| {
        b.order_key()
            .cmp(&a.order_key())
            .then_with(|| a.attribute.cmp(&b.attribute))
    });
    for fact in all {
        let mut status = fact.status.as_str().to_owned();
        if let Some(next) = &fact.superseded_by {
            status = format!("superseded by {next}");
        }
        let entry = format!(
            "**{}** · {} = {} · observed {}",
            fact.range().replace("..", " – "),
            cell(&fact.attribute),
            cell(&fact.display_value()),
            iso_date(fact.observed_at),
        );
        if fact.status == FactStatus::Retracted {
            out.push_str(&format!("- ~~{entry}~~ · retracted\n"));
        } else {
            out.push_str(&format!("- {entry} · {status}\n"));
        }
    }
    out
}

/// The point in time a question names, if any, as an `as_of` instant:
/// `as of 2025-06`, `in 2025`, `during 2024`, or what
/// [`parse_window`](crate::temporal::parse_window) understands (`in June
/// 2025`, `last month`, `yesterday`) — the **last** second of that period.
/// `None` when the question names no time.
#[must_use]
pub fn as_of_in_question(question: &str, now: i64) -> Option<i64> {
    let words: Vec<String> = question
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '?' | '!' | ';' | '(' | ')'))
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_end_matches('.').to_lowercase())
        .collect();
    for pair in words.windows(2) {
        if matches!(
            pair[0].as_str(),
            "in" | "of" | "during" | "until" | "at" | "on" | "by"
        ) && let Ok(date) = PartialDate::parse(&pair[1])
            && date.start() <= now
        {
            return Some(date.as_of_instant().min(now));
        }
    }
    crate::temporal::parse_window(question, now).map(|window| (window.end - 1).min(now))
}
