//! The pricing and budget math over the ledger's counts.
//!
//! Everything here is **pure and integer-only**: counts in, micro-USD out,
//! no clock and no database. Money is a `u64` of micro-USD — millionths of
//! a dollar, `1 USD = 1_000_000 micros` — because the prices are per
//! million tokens and the arithmetic stays in integers all the way; a
//! float here would round differently on every engine that touched it.
//!
//! A model with no entry in the price sheet is **unknown, not free**
//! ([`model_cost_micro_usd`] answers [`None`]): the budget counts what it
//! can price, and the endpoint says `null` rather than a zero that would
//! read as "this costs nothing". D1 queries are in the same position today
//! — [`BudgetStatus`] takes the *attributed* total, and attribution for D1
//! does not exist yet, so d1 contributes nothing to the budget; see the
//! module docs for why.

use time::OffsetDateTime;

use crate::store::{DayRow, Totals};

/// Micros in one dollar.
pub const MICRO_USD_PER_USD: u64 = 1_000_000;

/// Micro-USD per 1M tokens, input and output priced separately.
#[derive(Debug, Clone, Copy)]
struct Price {
    input: u64,
    output: u64,
}

// prices checked 2026-10, revise at billing. DeepSeek's own list prices
// per 1M tokens, in micro-USD: deepseek-chat $0.27 in / $1.10 out,
// deepseek-reasoner $0.55 in / $2.19 out. These are the model ids
// DeepSeek's API answers to, which is what a `model_connections` row
// stores in `model`.
const DEEPSEEK_CHAT: Price = Price {
    input: 270_000,
    output: 1_100_000,
};
const DEEPSEEK_REASONER: Price = Price {
    input: 550_000,
    output: 2_190_000,
};

/// What one email send is billed at: zero, because pilot volume sits well
/// inside the mail provider's free tier. One constant to revise when
/// billing applies — every send in the ledger is already counted, so the
/// math picks it up the moment the constant moves off zero.
pub const EMAIL_MICRO_USD_PER_SEND: u64 = 0;

/// The monthly infra budget when `USAGE_BUDGET_CENTS_MONTHLY` is unset:
/// 300 cents, the $3.00 infra share of the $9/mo Teams hosted plan at
/// roughly a two-thirds margin (pricing v3, docs/origin-and-plan.md).
pub const DEFAULT_BUDGET_CENTS_MONTHLY: u64 = 300;

/// The cost of one model call's tokens under the price sheet, in
/// micro-USD, or [`None`] when the `(provider, model)` pair is not priced.
///
/// The pair is what a `model_connections` row stores: the catalog's
/// provider id and the model id the provider answers to. The table is
/// keyed on both, so a reseller carrying `deepseek-chat` under another
/// provider id can get its own entry without moving DeepSeek's price.
///
/// Token counts are `u64` and the multiply runs in `u128`, so a record
/// cannot overflow its way into a wrong bill; the result saturates at
/// [`u64::MAX`] rather than wrapping, the same choice the harness's own
/// cost ledger makes.
#[must_use]
pub fn model_cost_micro_usd(
    provider: &str,
    model: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
) -> Option<u64> {
    let price = match (provider, model) {
        ("deepseek", "deepseek-chat") => DEEPSEEK_CHAT,
        ("deepseek", "deepseek-reasoner") => DEEPSEEK_REASONER,
        _ => return None,
    };
    // Per 1M tokens: micros = tokens * price / 1_000_000. Summing before
    // the divide keeps the remainder the caller would otherwise lose.
    let micros = (u128::from(prompt_tokens) * u128::from(price.input)
        + u128::from(completion_tokens) * u128::from(price.output))
        / 1_000_000;
    Some(u64::try_from(micros).unwrap_or(u64::MAX))
}

/// What `emails_sent` sends cost, in micro-USD.
#[must_use]
pub fn email_cost_micro_usd(emails_sent: u64) -> u64 {
    emails_sent.saturating_mul(EMAIL_MICRO_USD_PER_SEND)
}

/// Whether a month's usage fits the budget: [`BudgetStatus::Over`] once
/// the attributed month-to-date total is strictly past it,
/// [`BudgetStatus::Ok`] at or under. Exactly at the budget is not over —
/// the budget is a ceiling, and reaching it is what the ceiling is for.
///
/// `mtd_micros` is the **attributed** total: model cost when the
/// workspace's model is priced, plus email cost. D1 has no attribution
/// yet, so it contributes zero and a workspace could spend D1 all day
/// without this moving — that is a statement about what we can measure,
/// not about what D1 costs.
#[must_use]
pub fn status(mtd_micros: u64, budget_cents: u64) -> BudgetStatus {
    let budget_micros = u128::from(budget_cents) * 10_000;
    if u128::from(mtd_micros) > budget_micros {
        BudgetStatus::Over
    } else {
        BudgetStatus::Ok
    }
}

/// How a month stands against the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetStatus {
    /// At or under the budget.
    Ok,
    /// Strictly past the budget.
    Over,
}

impl BudgetStatus {
    /// The wire form the endpoint answers with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Over => "over",
        }
    }
}

/// The counts of the rows that fall in `now`'s UTC calendar month: the
/// month-to-date window the budget is checked against.
///
/// A `day` outside the window the table can hold (the store validates that
/// on write) simply does not match the month prefix, so this filters on
/// the `YYYY-MM` prefix and never parses a date.
#[must_use]
pub fn month_to_date(rows: &[DayRow], now: OffsetDateTime) -> Totals {
    let date = now.to_offset(time::UtcOffset::UTC).date();
    let prefix = format!("{:04}-{:02}", date.year(), u8::from(date.month()));
    let in_month: Vec<DayRow> = rows
        .iter()
        .filter(|row| row.day.starts_with(&prefix))
        .cloned()
        .collect();
    Totals::of(&in_month)
}

#[cfg(test)]
mod tests {
    use time::{Date, Month, Time, UtcOffset};

    use super::{
        BudgetStatus, EMAIL_MICRO_USD_PER_SEND, email_cost_micro_usd, model_cost_micro_usd,
        month_to_date, status,
    };
    use crate::store::DayRow;

    /// `2026-10-10T12:00:00Z`, the date in the issue's examples.
    fn at(day: u8) -> time::OffsetDateTime {
        time::OffsetDateTime::new_in_offset(
            Date::from_calendar_date(2026, Month::October, day).expect("a real date"),
            Time::MIDNIGHT,
            UtcOffset::UTC,
        )
    }

    fn row(day: &str, prompt: u64, completion: u64, emails: u64) -> DayRow {
        DayRow {
            day: day.to_owned(),
            model_requests: 1,
            prompt_tokens: prompt,
            completion_tokens: completion,
            d1_queries: 0,
            emails_sent: emails,
        }
    }

    /// The price-sheet math at full precision: exactly one million prompt
    /// tokens at $0.27/M is $0.27.
    #[test]
    fn a_million_prompt_tokens_price_their_entry() {
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-chat", 1_000_000, 0),
            Some(270_000)
        );
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-reasoner", 0, 1_000_000),
            Some(2_190_000)
        );
    }

    /// Sub-million counts truncate — integer math, never a float rounding
    /// that differs per engine.
    #[test]
    fn sub_million_counts_truncate_instead_of_rounding() {
        // 1_000_080_000 / 1_000_000 = 1000.
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-chat", 3_704, 0),
            Some(1_000)
        );
        // One token is 0 whole micros; the remainder is lost by design.
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-chat", 1, 0),
            Some(0)
        );
        // Sum before divide: the two remainders together can afford one.
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-chat", 2, 0),
            Some(0)
        );
    }

    /// A model the sheet does not carry is unknown, not free — on either
    /// axis of the pair.
    #[test]
    fn an_unpriced_model_is_none_not_zero() {
        assert_eq!(model_cost_micro_usd("deepseek", "gpt-4o", 100, 100), None);
        assert_eq!(
            model_cost_micro_usd("openai", "deepseek-chat", 100, 100),
            None
        );
        assert_eq!(model_cost_micro_usd("custom", "anything", 100, 100), None);
    }

    /// Absurd counts saturate instead of wrapping into a small bill.
    #[test]
    fn absurd_counts_saturate_rather_than_wrap() {
        let huge = u64::MAX;
        assert_eq!(
            model_cost_micro_usd("deepseek", "deepseek-chat", huge, huge),
            Some(u64::MAX)
        );
    }

    /// Email is free at pilot volume: the constant is the one thing to
    /// revise when billing applies.
    #[test]
    fn email_is_free_while_the_constant_is_zero() {
        assert_eq!(EMAIL_MICRO_USD_PER_SEND, 0);
        assert_eq!(email_cost_micro_usd(0), 0);
        assert_eq!(email_cost_micro_usd(50_000), 0);
    }

    /// The budget verdict: under is ok, exactly at is ok, past is over.
    #[test]
    fn the_budget_verdict_is_strictly_past_not_at() {
        assert_eq!(status(0, 300), BudgetStatus::Ok);
        assert_eq!(status(3_000_000, 300), BudgetStatus::Ok);
        assert_eq!(status(3_000_001, 300), BudgetStatus::Over);
        assert_eq!(status(3_000_000, 0), BudgetStatus::Over);
    }

    /// Month-to-date keeps this month's rows and drops the rest,
    /// including a row from the same day number last month.
    #[test]
    fn month_to_date_keeps_only_this_calendar_month() {
        let rows = [
            row("2026-09-30", 1, 0, 0),
            row("2026-10-01", 10, 0, 2),
            row("2026-10-10", 100, 7, 5),
        ];
        let mtd = month_to_date(&rows, at(10));
        assert_eq!(mtd.prompt_tokens, 110);
        assert_eq!(mtd.completion_tokens, 7);
        assert_eq!(mtd.emails_sent, 7);
        assert_eq!(mtd.model_requests, 2);
        assert_eq!(mtd.d1_queries, 0);
    }

    /// The window is the UTC calendar month of `now`, so nothing in an
    /// empty month and a year boundary does not leak.
    #[test]
    fn month_to_date_is_empty_in_an_unused_month() {
        let rows = [row("2025-12-31", 5, 5, 5), row("2026-11-01", 5, 5, 5)];
        let mtd = month_to_date(&rows, at(10));
        assert_eq!(mtd.prompt_tokens, 0);
        assert_eq!(mtd.emails_sent, 0);
    }
}
