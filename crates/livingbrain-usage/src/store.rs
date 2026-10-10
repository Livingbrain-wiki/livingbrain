//! The `usage_daily` table, behind the [`Database`](cratefield_core::Database)
//! port.
//!
//! Every statement is parameterized — no value is ever interpolated into
//! SQL — and every one names its workspace, so one workspace's ledger is
//! never readable as another's. The counters are written by **one atomic
//! upsert per record**: `INSERT ... ON CONFLICT DO UPDATE` that adds the
//! incoming counts to the day's row, so two callers recording in the same
//! day merge rather than one winning. `ON CONFLICT (…)` with `excluded` is
//! the same statement on SQLite and Postgres, which is why the increments
//! do not need a per-dialect rendering.
//!
//! The functions are free functions over `&dyn Database`, the way the
//! models store is; the caller (the handler, or a test) passes `now` in,
//! because a day key is a fact about *when something happened*, and a
//! record that must land on a past day's row (a retry after an outage)
//! should be able to say so rather than trust the store's wall clock.

use cratefield_core::{Database, DbError, Row, Statement};
use sea_query::Value as SeaValue;
use time::OffsetDateTime;

/// One day's usage facts for one workspace, as [`days`] hands them back.
///
/// All five counts are the table's own columns; there is no derived money
/// here. Pricing is [`cost`](crate::cost)'s job and runs over these counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DayRow {
    /// The UTC calendar day, `YYYY-MM-DD`.
    pub day: String,
    pub model_requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub d1_queries: u64,
    pub emails_sent: u64,
}

/// The five count columns summed over a window of [`DayRow`]s.
///
/// The shape the endpoint's `totals` object and the month-to-date figure
/// are both computed from: [`Totals::of`] is the sum, and the cost math in
/// [`crate::cost`] prices it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Totals {
    pub model_requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub d1_queries: u64,
    pub emails_sent: u64,
}

impl Totals {
    /// Sums the count columns of `rows`. An empty window is the all-zero
    /// total, not an error — a workspace that has used nothing is at zero,
    /// not missing.
    #[must_use]
    pub fn of(rows: &[DayRow]) -> Self {
        let mut totals = Self::default();
        for row in rows {
            totals.model_requests = totals.model_requests.saturating_add(row.model_requests);
            totals.prompt_tokens = totals.prompt_tokens.saturating_add(row.prompt_tokens);
            totals.completion_tokens = totals
                .completion_tokens
                .saturating_add(row.completion_tokens);
            totals.d1_queries = totals.d1_queries.saturating_add(row.d1_queries);
            totals.emails_sent = totals.emails_sent.saturating_add(row.emails_sent);
        }
        totals
    }
}

/// Everything a usage write or read can be refused for.
#[derive(Debug)]
pub enum UsageError {
    /// A workspace id outside the rule in [`is_workspace_id`].
    InvalidWorkspace(String),
    /// A day that is not a UTC `YYYY-MM-DD` calendar date.
    InvalidDay(String),
    /// A stored row that does not parse.
    Corrupt(String),
    /// The database failed.
    Store(DbError),
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWorkspace(value) => write!(
                f,
                "invalid workspace id `{value}`: non-empty, at most 128 bytes, no whitespace"
            ),
            Self::InvalidDay(value) => {
                write!(f, "invalid day `{value}`: expected a UTC YYYY-MM-DD date")
            }
            Self::Corrupt(message) => write!(f, "corrupt usage_daily row: {message}"),
            Self::Store(error) => write!(f, "store error: {error}"),
        }
    }
}

impl std::error::Error for UsageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

/// The workspace-id rule, the same one every module's store applies:
/// non-empty, at most 128 bytes, no whitespace.
#[must_use]
pub fn is_workspace_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_whitespace)
}

/// Whether `value` is a UTC calendar day, `YYYY-MM-DD`, naming a real
/// date. Lexicographic order on a `true` value is chronological order.
#[must_use]
pub fn is_day(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (
        digits(&bytes[0..4]),
        digits(&bytes[5..7]),
        digits(&bytes[8..10]),
    ) else {
        return false;
    };
    let (Ok(year), Ok(month), Ok(day)) = (
        i32::try_from(year),
        time::Month::try_from(month as u8),
        u8::try_from(day),
    ) else {
        return false;
    };
    time::Date::from_calendar_date(year, month, day).is_ok()
}

/// `Some(the digits as a number)` when every byte in `slice` is an ascii
/// digit, `None` otherwise.
fn digits(slice: &[u8]) -> Option<u32> {
    if slice.is_empty() {
        return None;
    }
    slice.iter().try_fold(0u32, |acc, byte| {
        byte.is_ascii_digit()
            .then_some(acc * 10 + u32::from(byte - b'0'))
    })
}

/// The UTC calendar day of `now`, `YYYY-MM-DD` — the storage format of the
/// `day` column.
#[must_use]
pub fn utc_day(now: OffsetDateTime) -> String {
    let date = now.to_offset(time::UtcOffset::UTC).date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Records model usage: one more request plus the two token counts.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`] and [`UsageError::Store`].
pub async fn record_model(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Result<(), UsageError> {
    record(
        db,
        now,
        workspace_id,
        Deltas {
            model_requests: 1,
            prompt_tokens,
            completion_tokens,
            ..Deltas::default()
        },
    )
    .await
}

/// Records one email sent.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`] and [`UsageError::Store`].
pub async fn record_email(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
) -> Result<(), UsageError> {
    record(
        db,
        now,
        workspace_id,
        Deltas {
            emails_sent: 1,
            ..Deltas::default()
        },
    )
    .await
}

/// Records D1 queries. A primitive for the future query-attribution hook;
/// nothing in the venture calls it yet, so the store tests are what keep
/// the increment honest.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`] and [`UsageError::Store`].
pub async fn record_d1(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    queries: u64,
) -> Result<(), UsageError> {
    record(
        db,
        now,
        workspace_id,
        Deltas {
            d1_queries: queries,
            ..Deltas::default()
        },
    )
    .await
}

/// The increments one record applies to the day's row.
#[derive(Default)]
struct Deltas {
    model_requests: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    d1_queries: u64,
    emails_sent: u64,
}

/// The one write: an upsert whose `DO UPDATE` adds every incoming count to
/// what the day already holds. Untouched columns arrive as zero, so adding
/// them changes nothing and one SQL template serves every record path.
///
/// The increment is atomic — the read of the existing row and the write of
/// the summed one are the same statement, so two concurrent records both
/// land instead of one overwriting the other.
async fn record(
    db: &dyn Database,
    now: OffsetDateTime,
    workspace_id: &str,
    deltas: Deltas,
) -> Result<(), UsageError> {
    if !is_workspace_id(workspace_id) {
        return Err(UsageError::InvalidWorkspace(workspace_id.to_owned()));
    }
    // A counter is an i64 in both dialects; a caller that hand us more
    // than that in one record is wrong by orders of magnitude, and
    // saturating refuses nothing — the day simply records all it can hold.
    let updated_at = now
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    db.execute(&Statement::with_values(
        "INSERT INTO usage_daily \
            (workspace_id, day, model_requests, prompt_tokens, completion_tokens, \
             d1_queries, emails_sent, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (workspace_id, day) DO UPDATE SET \
            model_requests = usage_daily.model_requests + excluded.model_requests, \
            prompt_tokens = usage_daily.prompt_tokens + excluded.prompt_tokens, \
            completion_tokens = usage_daily.completion_tokens + excluded.completion_tokens, \
            d1_queries = usage_daily.d1_queries + excluded.d1_queries, \
            emails_sent = usage_daily.emails_sent + excluded.emails_sent, \
            updated_at = excluded.updated_at",
        vec![
            text(workspace_id),
            text(&utc_day(now)),
            big(deltas.model_requests),
            big(deltas.prompt_tokens),
            big(deltas.completion_tokens),
            big(deltas.d1_queries),
            big(deltas.emails_sent),
            text(&updated_at),
        ],
    ))
    .await
    .map_err(UsageError::Store)?;
    Ok(())
}

/// Every day's row for one workspace between `first_day` and `last_day`
/// inclusive, oldest first. Both bounds are `YYYY-MM-DD` days and are
/// compared as strings — on that format, lexicographic order is
/// chronological order. A workspace with no usage in the window returns an
/// empty list, not an error: an unused ledger is an empty ledger.
///
/// # Errors
///
/// [`UsageError::InvalidWorkspace`], [`UsageError::InvalidDay`] for either
/// bound, and [`UsageError::Store`].
pub async fn days(
    db: &dyn Database,
    workspace_id: &str,
    first_day: &str,
    last_day: &str,
) -> Result<Vec<DayRow>, UsageError> {
    if !is_workspace_id(workspace_id) {
        return Err(UsageError::InvalidWorkspace(workspace_id.to_owned()));
    }
    if !is_day(first_day) {
        return Err(UsageError::InvalidDay(first_day.to_owned()));
    }
    if !is_day(last_day) {
        return Err(UsageError::InvalidDay(last_day.to_owned()));
    }
    let rows = db
        .query(&Statement::with_values(
            "SELECT day, model_requests, prompt_tokens, completion_tokens, d1_queries, \
                    emails_sent \
             FROM usage_daily WHERE workspace_id = ? AND day >= ? AND day <= ? ORDER BY day",
            vec![text(workspace_id), text(first_day), text(last_day)],
        ))
        .await
        .map_err(UsageError::Store)?;
    rows.rows.iter().map(day_from).collect()
}

fn day_from(row: &Row) -> Result<DayRow, UsageError> {
    Ok(DayRow {
        day: row
            .get::<String>("day")
            .ok_or_else(|| UsageError::Corrupt("day is not text".to_owned()))?,
        model_requests: count(row, "model_requests")?,
        prompt_tokens: count(row, "prompt_tokens")?,
        completion_tokens: count(row, "completion_tokens")?,
        d1_queries: count(row, "d1_queries")?,
        emails_sent: count(row, "emails_sent")?,
    })
}

fn count(row: &Row, column: &str) -> Result<u64, UsageError> {
    row.get::<u64>(column)
        .ok_or_else(|| UsageError::Corrupt(format!("{column} is not a counter")))
}

fn text(value: &str) -> SeaValue {
    SeaValue::String(Some(Box::new(value.to_owned())))
}

/// A counter bound as `BIGINT`, the only integer binding the portable
/// subset needs — Postgres runs it into a BIGINT column and SQLite into
/// its 64-bit INTEGER.
fn big(value: u64) -> SeaValue {
    SeaValue::BigInt(Some(i64::try_from(value).unwrap_or(i64::MAX)))
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, Time, UtcOffset};

    use super::{is_day, is_workspace_id, utc_day};

    fn instant(year: i32, month: Month, day: u8, hour: u8) -> time::OffsetDateTime {
        time::OffsetDateTime::new_in_offset(
            Date::from_calendar_date(year, month, day).expect("a real date"),
            Time::from_hms(hour, 0, 0).expect("a real time"),
            UtcOffset::UTC,
        )
    }

    /// The day key is zero-padded `YYYY-MM-DD`, so lexicographic order is
    /// chronological order — the property the range query leans on.
    #[test]
    fn utc_day_is_zero_padded_iso() {
        assert_eq!(utc_day(instant(2026, Month::October, 10, 0)), "2026-10-10");
        assert_eq!(utc_day(instant(2026, Month::January, 3, 23)), "2026-01-03");
        assert_eq!(utc_day(instant(2027, Month::January, 15, 8)), "2027-01-15");
    }

    /// The day key is the UTC day, not the wall-clock day of whatever
    /// offset the instant is rendered in: 20:00 UTC is already tomorrow at
    /// +14, and the key still says today.
    #[test]
    fn utc_day_is_the_utc_calendar_day_not_the_local_one() {
        let utc_evening = instant(2026, Month::October, 10, 20);
        let ahead = utc_evening.to_offset(UtcOffset::from_hms(14, 0, 0).expect("a real offset"));
        assert_eq!(ahead.date().day(), 11, "the local wall clock says tomorrow");
        assert_eq!(utc_day(ahead), "2026-10-10", "the key stays on the UTC day");
    }

    /// Only real calendar dates pass: shape, ranges, month lengths,
    /// leap years.
    #[test]
    fn is_day_accepts_real_dates_only() {
        assert!(is_day("2026-10-10"));
        assert!(is_day("2024-02-29"), "a leap day");
        assert!(!is_day("2026-02-29"), "2026 is not a leap year");
        assert!(!is_day("2026-13-01"));
        assert!(!is_day("2026-00-10"));
        assert!(!is_day("2026-04-31"));
        assert!(!is_day("2026-10-0"));
        assert!(!is_day("26-10-10"));
        assert!(!is_day("2026/10/10"));
        assert!(!is_day("2026-10-10T00:00:00Z"));
        assert!(!is_day(""));
        assert!(!is_day("not-a-date"));
    }

    /// The workspace-id rule, the same one every module applies.
    #[test]
    fn is_workspace_id_follows_the_shared_rule() {
        assert!(is_workspace_id("T0SPACE"));
        assert!(is_workspace_id(&"a".repeat(128)));
        assert!(!is_workspace_id(""));
        assert!(!is_workspace_id("T0 SPACE"));
        assert!(!is_workspace_id(&"a".repeat(129)));
        assert!(!is_workspace_id("tab\tid"));
    }
}
