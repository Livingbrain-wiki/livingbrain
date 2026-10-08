//! Time windows: the one place in the crate that knows what a calendar is.
//!
//! Everything else works in unix seconds ([`Fact::at`](crate::Fact::at), a
//! `i64`), which is what the rest of the workspace stores and what no date
//! library is needed to sort. What a date library *is* needed for is turning
//! "in June" into two integers, so the civil-date arithmetic is written out
//! longhand here: no dependency, no time zone, no `Local`.

/// Seconds in a day. UTC days, since a memory's `at` has no zone attached.
pub const DAY: i64 = 86_400;

/// A half-open span of unix seconds, `[start, end)` — so adjacent days tile
/// without overlapping and no fact is in both, or in neither.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first second in the window, inclusive.
    pub start: i64,
    /// The first second after the window, exclusive.
    pub end: i64,
}

impl Window {
    #[must_use]
    pub fn contains(&self, at: i64) -> bool {
        at >= self.start && at < self.end
    }

    /// The window's length in seconds, at least one.
    #[must_use]
    pub fn len_secs(&self) -> i64 {
        self.end.saturating_sub(self.start).max(1)
    }
}

/// Month names and abbreviations, index 0 = January.
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// The month a name refers to, or `None`. Both the full name and its first
/// three letters match, so "Jun" and "june" are the same month.
fn month_of(word: &str) -> Option<u32> {
    MONTHS
        .iter()
        .position(|name| *name == word || (word.len() == 3 && name.starts_with(word)))
        .map(|index| index as u32 + 1)
}

/// Days since the Unix epoch for a civil date (Howard Hinnant's algorithm,
/// which is exact for any year this crate will ever see).
#[must_use]
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * i64::from(shifted) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The civil date `(year, month, day)` of a day count since the epoch — the
/// inverse of [`days_from_civil`].
#[must_use]
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Midnight UTC of the day `at` falls in, saturating rather than wrapping at
/// the ends of the `i64` range: an instant nobody could have meant is a
/// question this crate answers with nothing, not with a panic.
#[must_use]
pub const fn start_of_day(at: i64) -> i64 {
    at.div_euclid(DAY).saturating_mul(DAY)
}

/// `at` as `YYYY-MM-DD`, the date form facts are recalled and rendered in.
#[must_use]
pub fn iso_date(at: i64) -> String {
    let (year, month, day) = civil_from_days(at.div_euclid(DAY));
    format!("{year:04}-{month:02}-{day:02}")
}

/// Midnight UTC of the first day of `(year, month)`; month 13 is January of
/// the next year, which is how every caller says "the month after this one".
/// `None` for a year whose day count is not an `i64` of seconds.
fn start_of_month(year: i64, month: u32) -> Option<i64> {
    let (year, month) = if month > 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month)
    };
    days_from_civil(year, month, 1).checked_mul(DAY)
}

/// The whole of month `month` in `year`.
fn month_window(year: i64, month: u32) -> Option<Window> {
    Some(Window {
        start: start_of_month(year, month)?,
        end: start_of_month(year, month + 1)?,
    })
}

/// The most recent occurrence of `month` that has not started after `now`.
///
/// "In June" asked in July is June this year; asked in March of the same year
/// it is June *last* year, because a window in the future contains nothing and
/// an empty answer to a question about June is worse than one about the wrong
/// June. The walk is over month indices rather than a year and a month
/// separately, so it cannot step the month back without stepping the year
/// back too, and it is bounded to twelve steps.
fn most_recent_month(month: u32, now: i64) -> Option<Window> {
    let (year, now_month, _) = civil_from_days(start_of_day(now) / DAY);
    let this_month = year
        .checked_mul(12)?
        .checked_add(i64::from(now_month) - 1)?;
    for back in 0..=12 {
        let Some(index) = this_month.checked_sub(i64::from(back)) else {
            break;
        };
        let candidate_month = index.rem_euclid(12) as u32 + 1;
        if candidate_month == month
            && let Some(window) = month_window(index.div_euclid(12), candidate_month)
            && window.start <= now
        {
            return Some(window);
        }
    }
    month_window(year.checked_sub(1)?, month)
}

/// Parse the time window a question asks about, or `None` if it names none.
///
/// Understood, all relative to `now` (unix seconds):
///
/// * `today`, `yesterday`
/// * `last week` (the seven days before today), `last month` (the previous
///   calendar month), `last sprint` (the fourteen days before today)
/// * a month name, alone or with a year: `in June`, `june`, `June 2025`.
///   A window that has not started by `now` is not parsed at all.
///
/// The relative phrases are checked first, because `last month` contains the
/// word `month` and a month name would otherwise eat it. A query with no time
/// in it is not a failed query — it is a query about all of time, and says
/// [`None`] so the caller leaves the ordering alone.
#[must_use]
pub fn parse_window(text: &str, now: i64) -> Option<Window> {
    let words = crate::text::tokenize(text);
    let has = |needle: &str| words.iter().any(|word| word == needle);

    let today = start_of_day(now);
    if has("today") {
        return Some(Window {
            start: today,
            end: today.saturating_add(DAY),
        });
    }
    if has("yesterday") {
        return Some(Window {
            start: today.saturating_sub(DAY),
            end: today,
        });
    }
    if has("sprint") {
        return Some(Window {
            start: today.saturating_sub(14 * DAY),
            end: today.saturating_add(DAY),
        });
    }
    if has("week") {
        return Some(Window {
            start: today.saturating_sub(7 * DAY),
            end: today.saturating_add(DAY),
        });
    }
    if has("month") {
        let (year, month, _) = civil_from_days(today / DAY);
        return month_window(
            year.checked_sub(i64::from(month == 1))?,
            if month == 1 { 12 } else { month - 1 },
        );
    }
    let index = words.iter().position(|word| month_of(word).is_some())?;
    let month = month_of(&words[index])?;
    // A four-digit year next to the name is explicit; without one, the most
    // recent such month is meant.
    let year = words.get(index + 1).and_then(|next| {
        if next.len() == 4 {
            next.parse::<i64>()
                .ok()
                .filter(|year| (1970..=9999).contains(year))
        } else {
            None
        }
    });
    let window = match year {
        Some(year) => month_window(year, month)?,
        None => most_recent_month(month, now)?,
    };
    // A window entirely in the future is not parsed: the answer to "what did
    // we do in December 2027" asked in 2026 is not an empty one, and refusing
    // the window leaves the question to be answered over all of time rather
    // than answered wrongly with silence.
    (window.start <= now).then_some(window)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Noon on a Monday in June 2026 — written as a date rather than a magic
    /// number so the expectations below read as calendar facts.
    fn now() -> i64 {
        days_from_civil(2026, 6, 15) * DAY + 12 * 3_600
    }

    #[test]
    fn june_window_is_the_whole_month() {
        let window = parse_window("What happened in June?", now()).expect("a window");
        assert_eq!(iso_date(window.start), "2026-06-01");
        assert_eq!(iso_date(window.end - 1), "2026-06-30");
    }

    #[test]
    fn month_with_a_year_is_that_year() {
        let window = parse_window("notes from june 2024", now()).expect("a window");
        assert_eq!(iso_date(window.start), "2024-06-01");
    }

    #[test]
    fn a_future_month_falls_back_to_last_year() {
        let march = days_from_civil(2026, 3, 15) * DAY;
        let window = parse_window("what about june", march).expect("a window");
        assert_eq!(iso_date(window.start), "2025-06-01");
    }

    #[test]
    fn relative_phrases_beat_month_names() {
        let today = parse_window("what did we do today", now()).expect("a window");
        assert_eq!(today.len_secs(), DAY);
        let week = parse_window("last week", now()).expect("a window");
        assert_eq!(iso_date(week.start), "2026-06-08");
        let month = parse_window("last month", now()).expect("a window");
        assert_eq!(iso_date(month.start), "2026-05-01");
        let sprint = parse_window("last sprint", now()).expect("a window");
        assert_eq!(iso_date(sprint.start), "2026-06-01");
        let yesterday = parse_window("yesterday", now()).expect("a window");
        assert_eq!(iso_date(yesterday.start), "2026-06-14");
    }

    #[test]
    fn a_window_still_to_come_is_refused() {
        assert_eq!(parse_window("notes from dec 2027", now()), None);
    }

    #[test]
    fn a_clock_at_the_edge_of_the_range_does_not_panic() {
        for extreme in [i64::MAX, i64::MAX - 1, i64::MIN, i64::MIN + 1, -1, 0] {
            for text in [
                "what happened today",
                "yesterday",
                "last week",
                "last month",
                "last sprint",
                "in june",
                "june 2024",
            ] {
                let _ = parse_window(text, extreme);
            }
        }
    }

    #[test]
    fn a_question_with_no_time_has_no_window() {
        assert_eq!(parse_window("where does Ada live", now()), None);
    }

    #[test]
    fn civil_dates_round_trip() {
        for days in [-25_000, -1, 0, 1, 20_000, 90_000] {
            assert_eq!(
                days_from_civil(
                    civil_from_days(days).0,
                    civil_from_days(days).1,
                    civil_from_days(days).2
                ),
                days
            );
        }
    }
}
