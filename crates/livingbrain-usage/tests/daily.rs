//! `GET /v1/usage/daily`: who may read the bill, and what the answer
//! looks like — the counts, the pricing decision, the budget verdict.
//!
//! The acceptance shape is the one issue #12 sketches: 401 with no
//! credential, 403 for a member who is not an admin, and a 200 whose body
//! carries the workspace's days, their totals, the month-to-date cost and
//! the budget verdict.

mod support;

use cratefield_core::axum::http::StatusCode;
use serde_json::{Value, json};
use support::{OWNER, PLAIN, WORKSPACE, get, get_with_token};

/// The day the kit's fixed clock sits on, `YYYY-MM-DD`.
fn today() -> String {
    livingbrain_usage::utc_day(support::now())
}

/// An empty ledger is a 200 with zeroed totals, an empty day list, no
/// model cost — nothing is connected, so there is no price sheet entry to
/// price with — and status ok at the default $3 budget.
#[test]
fn an_empty_ledger_reads_as_zeros_ok() {
    pollster::block_on(async {
        let (kit, cookie) = support::signed_in().await;

        let res = get(&kit, "/v1/usage/daily", &cookie).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(
            res.json(),
            json!({
                "workspace_id": WORKSPACE,
                "days": [],
                "totals": {
                    "model_requests": 0,
                    "prompt_tokens": 0,
                    "completion_tokens": 0,
                    "d1_queries": 0,
                    "emails_sent": 0,
                    "model_usd_micros": null,
                    "email_usd_micros": 0
                },
                "month_to_date_usd_micros": 0,
                "budget_cents": 300,
                "status": "ok"
            }),
            "{}",
            res.text()
        );
    });
}

/// A request with no credential is a 401 before anything else happens.
#[test]
fn a_request_without_a_credential_is_refused() {
    pollster::block_on(async {
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;

        let res = get(&kit, "/v1/usage/daily", "").await;

        assert_eq!(res.status, StatusCode::UNAUTHORIZED, "{}", res.text());
    });
}

/// A member who is neither admin nor owner sees 403, with the slug that
/// names why.
#[test]
fn a_non_admin_member_is_refused() {
    pollster::block_on(async {
        let (kit, owner_cookie) = support::signed_in().await;
        let plain_cookie = support::session(&kit, WORKSPACE, PLAIN);

        let res = get(&kit, "/v1/usage/daily", &plain_cookie).await;

        assert_eq!(res.status, StatusCode::FORBIDDEN, "{}", res.text());
        assert!(res.text().contains("usage/forbidden"), "{}", res.text());
        // The owner's own cookie still works: the refusal is about the
        // caller, not the route.
        let ok = get(&kit, "/v1/usage/daily", &owner_cookie).await;
        assert_eq!(ok.status, StatusCode::OK, "{}", ok.text());
    });
}

/// A personal access token opens the same ledger the session cookie does,
/// through the one `authenticate` every route shares.
#[test]
fn a_personal_access_token_opens_the_ledger() {
    pollster::block_on(async {
        let (kit, cookie) = support::signed_in().await;
        let token = support::mint_token(&kit, &cookie).await;

        let res = get_with_token(&kit, "/v1/usage/daily", &token).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["workspace_id"], json!(WORKSPACE));
    });
}

/// Recorded usage shows up as days and totals: two model records and an
/// email in the default window are one day's row, summed, with email cost
/// zero while the pilot's mail tier is free.
#[test]
fn recorded_usage_arrives_as_days_and_totals() {
    pollster::block_on(async {
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 1_000, 300)
            .await
            .expect("the record lands");
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 500, 100)
            .await
            .expect("the record lands");
        livingbrain_usage::record_email(&*kit.db, support::now(), WORKSPACE)
            .await
            .expect("the send lands");

        let res = get(
            &kit,
            "/v1/usage/daily",
            &support::session(&kit, WORKSPACE, OWNER),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(
            body["days"],
            json!([{
                "day": today(),
                "model_requests": 2,
                "prompt_tokens": 1_500,
                "completion_tokens": 400,
                "d1_queries": 0,
                "emails_sent": 1
            }]),
            "{}",
            res.text()
        );
        assert_eq!(body["totals"]["prompt_tokens"], json!(1_500));
        assert_eq!(body["totals"]["model_requests"], json!(2));
        assert_eq!(body["totals"]["emails_sent"], json!(1));
        assert_eq!(body["totals"]["email_usd_micros"], json!(0));
    });
}

/// A priced main model turns the tokens into micro-USD; an unpriced one
/// is `null` — unknown, not free. Neither touches the other's numbers.
#[test]
fn the_main_model_decides_whether_model_cost_is_a_number() {
    pollster::block_on(async {
        // DeepSeek's main role is priced: the day's tokens have a cost.
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "deepseek", "deepseek-chat").await;
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 1_000_000, 0)
            .await
            .expect("the record lands");
        let cookie = support::session(&kit, WORKSPACE, OWNER);

        let res = get(&kit, "/v1/usage/daily", &cookie).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["totals"]["model_usd_micros"], json!(270_000));
        assert_eq!(res.json()["month_to_date_usd_micros"], json!(270_000));

        // The same usage under a model the sheet does not carry is
        // unknown cost: null, and the budget reads zero attributed.
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "openai", "gpt-5").await;
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 1_000_000, 0)
            .await
            .expect("the record lands");
        let cookie = support::session(&kit, WORKSPACE, OWNER);

        let res = get(&kit, "/v1/usage/daily", &cookie).await;
        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["totals"]["model_usd_micros"], Value::Null);
        assert_eq!(res.json()["month_to_date_usd_micros"], json!(0));
    });
}

/// Past the budget the verdict is "over": a $1 budget (the env var is in
/// cents) against $2.70 of DeepSeek tokens.
#[test]
fn a_month_past_the_budget_reads_over() {
    pollster::block_on(async {
        let kit = support::harness_with_budget("100");
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "deepseek", "deepseek-chat").await;
        // 4_000_000 * 270_000 / 1_000_000 = 1_080_000 micros — $1.08
        // against the $1.00 the 100-cent budget names.
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 4_000_000, 0)
            .await
            .expect("the record lands");

        let res = get(
            &kit,
            "/v1/usage/daily",
            &support::session(&kit, WORKSPACE, OWNER),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(res.json()["budget_cents"], json!(100));
        assert_eq!(res.json()["month_to_date_usd_micros"], json!(1_080_000));
        assert_eq!(res.json()["status"], json!("over"));
    });
}

/// Exactly at the budget is not over: the budget is a ceiling, and
/// reaching it is what the ceiling is for. Integer division truncates, so
/// the closest a ledger can land under 100 cents is one micro short.
#[test]
fn a_month_exactly_at_the_budget_is_still_ok() {
    pollster::block_on(async {
        let kit = support::harness_with_budget("100");
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "deepseek", "deepseek-reasoner").await;
        // 1_818_181 * 550_000 / 1_000_000 = 999_999 micros — one short of
        // the 1_000_000 the 100-cent budget names.
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 1_818_181, 0)
            .await
            .expect("the record lands");

        let res = get(
            &kit,
            "/v1/usage/daily",
            &support::session(&kit, WORKSPACE, OWNER),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        assert_eq!(
            res.json()["month_to_date_usd_micros"],
            json!(999_999),
            "1_818_181 * 550_000 / 1_000_000"
        );
        assert_eq!(res.json()["status"], json!("ok"));
    });
}

/// The window clamps: `days=0` and `days=999` answer, within 1..=31;
/// garbage is a validation failure naming the parameter.
#[test]
fn the_days_window_clamps_to_one_through_thirty_one() {
    pollster::block_on(async {
        let (kit, cookie) = support::signed_in().await;

        let zero = get(&kit, "/v1/usage/daily?days=0", &cookie).await;
        assert_eq!(zero.status, StatusCode::OK, "{}", zero.text());
        assert_eq!(zero.json()["days"].as_array().map(Vec::len), Some(0));

        let huge = get(&kit, "/v1/usage/daily?days=999", &cookie).await;
        assert_eq!(huge.status, StatusCode::OK, "{}", huge.text());

        let garbage = get(&kit, "/v1/usage/daily?days=lots", &cookie).await;
        assert_eq!(
            garbage.status,
            StatusCode::BAD_REQUEST,
            "{}",
            garbage.text()
        );
        assert!(
            garbage.text().contains("days"),
            "the problem names the parameter: {}",
            garbage.text()
        );
    });
}

/// A record from last month is outside the month-to-date window even
/// when the day list shows it: the budget reads this month only.
#[test]
fn month_to_date_ignores_last_months_rows() {
    pollster::block_on(async {
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "deepseek", "deepseek-chat").await;
        // Last month: $2.70 of tokens, inside the window the ledger
        // still holds (the kit's clock sits at 2027-01-15, so a month ago
        // is 2026-12).
        let last_month = support::now() - time::Duration::days(30);
        livingbrain_usage::record_model(&*kit.db, last_month, WORKSPACE, 1_000_000, 0)
            .await
            .expect("the old record lands");
        // This month: $0.02.
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 100_000, 0)
            .await
            .expect("the record lands");
        let cookie = support::session(&kit, WORKSPACE, OWNER);

        let res = get(&kit, "/v1/usage/daily?days=31", &cookie).await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["days"].as_array().map(Vec::len), Some(2));
        // Window total: both months. Month-to-date: this month only.
        assert_eq!(body["totals"]["model_usd_micros"], json!(297_000));
        assert_eq!(body["month_to_date_usd_micros"], json!(27_000));
        assert_eq!(body["status"], json!("ok"));
    });
}

/// Month-to-date is the calendar month, not the caller's window: usage
/// from earlier in the month, outside a short `days=`, still counts
/// toward the budget. Before the endpoint read a second, month-wide
/// window, this spend was invisible and the month read "ok" while over.
#[test]
fn month_to_date_covers_the_month_not_just_the_window() {
    pollster::block_on(async {
        let kit = support::harness_with_budget("100");
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        support::seed_main_model(&kit, "deepseek", "deepseek-chat").await;
        // $1.08 of tokens on the month's day 2 — the kit's clock sits at
        // 2027-01-15, so `days=7` reaches back only to 2027-01-09 and this
        // row is outside the served window but inside the month.
        let day_two = support::now() - time::Duration::days(13);
        livingbrain_usage::record_model(&*kit.db, day_two, WORKSPACE, 4_000_000, 0)
            .await
            .expect("the record lands");

        let res = get(
            &kit,
            "/v1/usage/daily?days=7",
            &support::session(&kit, WORKSPACE, OWNER),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(
            body["days"].as_array().map(Vec::len),
            Some(0),
            "the served window is the last 7 days; day 2 is not in it: {}",
            res.text()
        );
        // The window total is zero — no usage inside it — while the month
        // carries the day-2 spend, and the verdict follows the month.
        assert_eq!(body["totals"]["model_usd_micros"], json!(0));
        assert_eq!(body["month_to_date_usd_micros"], json!(1_080_000));
        assert_eq!(body["status"], json!("over"));
    });
}

/// One workspace's ledger never reads another's: both workspaces have
/// rows — the foreign one's much bigger — and the answer to A's admin
/// carries only A's.
#[test]
fn a_workspace_never_sees_another_workspaces_rows() {
    pollster::block_on(async {
        let kit = support::harness();
        support::seed_workspace(&kit, WORKSPACE, OWNER).await;
        support::seed_member(&kit, WORKSPACE, OWNER, true).await;
        const OTHER: &str = "T0OTHER";
        support::seed_workspace(&kit, OTHER, "U0OTHER").await;
        support::seed_member(&kit, OTHER, "U0OTHER", true).await;
        // A's day is small; the foreign day is two orders bigger, so a
        // leak of B's rows shows up in A's totals.
        livingbrain_usage::record_model(&*kit.db, support::now(), WORKSPACE, 1_000, 100)
            .await
            .expect("the record lands");
        livingbrain_usage::record_model(&*kit.db, support::now(), OTHER, 9_000_000, 900_000)
            .await
            .expect("the foreign record lands");

        let res = get(
            &kit,
            "/v1/usage/daily",
            &support::session(&kit, WORKSPACE, OWNER),
        )
        .await;

        assert_eq!(res.status, StatusCode::OK, "{}", res.text());
        let body = res.json();
        assert_eq!(body["workspace_id"], json!(WORKSPACE));
        assert_eq!(
            body["days"],
            json!([{
                "day": today(),
                "model_requests": 1,
                "prompt_tokens": 1_000,
                "completion_tokens": 100,
                "d1_queries": 0,
                "emails_sent": 0
            }]),
            "{}",
            res.text()
        );
        assert_eq!(body["totals"]["prompt_tokens"], json!(1_000));
        assert_eq!(body["totals"]["completion_tokens"], json!(100));
        assert_eq!(body["totals"]["model_requests"], json!(1));
    });
}
