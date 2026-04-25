//! Cost-forecast + platform-pricing endpoints.
//!
//! Both endpoints feed the billing / chargeback story directly:
//!
//!   - `GET /api/analytics/cost-forecast` — month-to-date USD spend,
//!     linear extrapolation to month-end, prior-month delta. Read by
//!     the dashboard "↑ N% vs last month" trend chip.
//!   - `GET/PATCH /api/admin/platform-pricing` — singleton baseline
//!     `(input_price_per_token, output_price_per_token, currency)`.
//!     Per-model weights multiply against this; mis-typing it
//!     directly mis-bills every request.
//!
//! Neither had a dedicated test before this file. The contract this
//! pins down:
//!
//!   1. Forecast envelope keys never silently rename (dashboard
//!      reads them by name).
//!   2. Linear extrapolation math: `mtd * days_in / day_elapsed`.
//!   3. `prior_month_same_window_usd` is null when last month has
//!      no spend in the same window — first-month deployments
//!      shouldn't show "↑ Inf%" trend chips.
//!   4. Platform pricing GET/PATCH round-trips Decimal values
//!      losslessly (Decimal-end-to-end is the whole point of the
//!      cost stack — see `cost_decimal` migration in main).
//!   5. Negative pricing is rejected at validation.
//!   6. PATCH emits a `platform_pricing.updated` audit row carrying
//!      the new values in `detail`.

use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use think_watch_test_support::prelude::*;

async fn admin_session(app: &TestApp) -> TestClient {
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();
    con
}

// ---------------------------------------------------------------------------
// Cost forecast
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn cost_forecast_returns_full_envelope_on_empty_clickhouse() {
    // Fresh deployment: no `gateway_logs` rows, both windows are zero.
    // The handler must still return a well-formed envelope with the
    // numeric fields zeroed and the trend fields nulled (not
    // serializing `NaN` or `Infinity` — neither is valid JSON).
    let app = TestApp::spawn_with_clickhouse().await;
    let con = admin_session(&app).await;

    let body: Value = con
        .get("/api/analytics/cost-forecast")
        .await
        .unwrap()
        .json()
        .unwrap();

    // Envelope shape — every field present.
    for k in [
        "month_to_date_usd",
        "days_elapsed",
        "days_in_month",
        "projected_month_end_usd",
        "prior_month_same_window_usd",
        "trend_pct",
    ] {
        assert!(
            body.get(k).is_some(),
            "field {k} missing from cost-forecast envelope: {body}"
        );
    }
    assert_eq!(body["month_to_date_usd"].as_f64(), Some(0.0));
    assert_eq!(body["projected_month_end_usd"].as_f64(), Some(0.0));
    // Empty prior-month window → null. Without this, the dashboard
    // shows "↑ NaN%" or "↑ Inf%" — both are JSON-invalid and the
    // client crashes anyway.
    assert!(
        body["prior_month_same_window_usd"].is_null(),
        "prior must be null on empty CH, got {body}"
    );
    assert!(
        body["trend_pct"].is_null(),
        "trend_pct must be null when prior is null, got {body}"
    );

    // days_in_month is 28..=31; days_elapsed is 1..=days_in_month.
    let days_in = body["days_in_month"].as_u64().unwrap();
    let days_elapsed = body["days_elapsed"].as_u64().unwrap();
    assert!(
        (28..=31).contains(&days_in),
        "days_in_month must be 28..=31: {days_in}"
    );
    assert!(
        (1..=days_in).contains(&days_elapsed),
        "days_elapsed must be in [1, days_in_month]: {days_elapsed} / {days_in}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn cost_forecast_extrapolates_linear_run_rate() {
    // Drive a real gateway call so the audit pipeline writes a
    // properly-encoded `gateway_logs` row (Decimal at scale 10),
    // then verify the projection follows the documented formula:
    //   projected = mtd * days_in_month / days_elapsed
    //
    // We can't INSERT directly because raw `i64`-into-Decimal
    // encoding has to round-trip through `cost_decimal::encode_i64`
    // exactly the way the production audit path does — easier to
    // just let the production path do it.
    let app = TestApp::spawn_with_clickhouse().await;
    let user = fixtures::create_random_user(&app.db).await.unwrap();
    let upstream = MockProvider::openai_chat_ok("forecast-model").await;
    let uri = upstream.uri();
    Box::leak(Box::new(upstream));
    let provider =
        fixtures::create_provider(&app.db, &unique_name("fc-prov"), "openai", &uri, None)
            .await
            .unwrap();
    fixtures::create_model_and_route(&app.db, provider.id, "forecast-model")
        .await
        .unwrap();
    app.rebuild_gateway_router().await;
    let key = fixtures::create_api_key(
        &app.db,
        user.user.id,
        &unique_name("fc-key"),
        &["ai_gateway"],
        None,
        None,
    )
    .await
    .unwrap();
    let gw = app.gateway_client();
    gw.set_bearer(&key.plaintext);
    gw.post(
        "/v1/chat/completions",
        json!({"model": "forecast-model", "messages": [{"role": "user", "content": "x"}]}),
    )
    .await
    .unwrap()
    .assert_ok();

    // Wait for the row to land in CH.
    let ch = app.state.clickhouse.as_ref().unwrap();
    for _ in 0..200 {
        let n: u64 = ch
            .query("SELECT count() FROM gateway_logs WHERE cost_usd IS NOT NULL")
            .fetch_one()
            .await
            .unwrap_or(0);
        if n > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let con = admin_session(&app).await;
    let body: Value = con
        .get("/api/analytics/cost-forecast")
        .await
        .unwrap()
        .json()
        .unwrap();

    let mtd = body["month_to_date_usd"].as_f64().unwrap();
    let days_in = body["days_in_month"].as_f64().unwrap();
    let days_elapsed = body["days_elapsed"].as_f64().unwrap();
    let projected = body["projected_month_end_usd"].as_f64().unwrap();

    assert!(mtd > 0.0, "MTD should reflect the gateway call: {mtd}");
    let expected = mtd * days_in / days_elapsed;
    let _ = Decimal::from_str("0").unwrap(); // keep rust_decimal import live
    assert!(
        (projected - expected).abs() < 0.0001,
        "projected ({projected}) must equal mtd*days_in/days_elapsed ({expected})"
    );
    assert!(
        projected >= mtd,
        "projection must extrapolate FORWARD from MTD: projected={projected} mtd={mtd}"
    );
}

// ---------------------------------------------------------------------------
// Platform pricing
// ---------------------------------------------------------------------------

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn platform_pricing_get_returns_seeded_singleton() {
    // The migration seeds a single `(id=1)` row. GET must always
    // succeed — admin UI on a fresh deployment shows the form
    // pre-filled, and a 500 here would white-screen the page.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let body: Value = con
        .get("/api/admin/platform-pricing")
        .await
        .unwrap()
        .json()
        .unwrap();

    for k in [
        "input_price_per_token",
        "output_price_per_token",
        "currency",
    ] {
        assert!(
            body.get(k).is_some(),
            "platform_pricing envelope missing {k}: {body}"
        );
    }
    // `currency` is a 3-letter ISO code by convention.
    let cur = body["currency"].as_str().expect("currency is a string");
    assert!(
        !cur.is_empty() && cur.len() <= 8,
        "currency code looks bogus: {cur:?}"
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn platform_pricing_patch_round_trips_decimal_precision() {
    // Decimal-end-to-end is the whole point of the cost stack. A
    // PATCH with a tiny fractional value (sub-cent token pricing
    // is realistic — cheapest commercial model is ~$1.5e-7/token)
    // must round-trip without f64 erosion.
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let new_input = "0.0000001500"; // $1.5e-7
    let new_output = "0.0000006000"; // $6e-7
    let resp = con
        .patch(
            "/api/admin/platform-pricing",
            json!({
                "input_price_per_token": new_input,
                "output_price_per_token": new_output,
                "currency": "USD",
            }),
        )
        .await
        .unwrap();
    resp.assert_ok();
    let updated: Value = resp.json().unwrap();
    // The values come back as strings or numbers depending on the
    // serializer; either way, parsing as Decimal must yield the
    // exact input.
    let got_input = updated["input_price_per_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            updated["input_price_per_token"]
                .as_f64()
                .map(|f| f.to_string())
        })
        .expect("input_price_per_token in response");
    let got_output = updated["output_price_per_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            updated["output_price_per_token"]
                .as_f64()
                .map(|f| f.to_string())
        })
        .expect("output_price_per_token in response");
    assert_eq!(
        Decimal::from_str(&got_input).unwrap(),
        Decimal::from_str(new_input).unwrap(),
        "input price must round-trip without erosion: got {got_input}"
    );
    assert_eq!(
        Decimal::from_str(&got_output).unwrap(),
        Decimal::from_str(new_output).unwrap(),
        "output price must round-trip without erosion: got {got_output}"
    );
    assert_eq!(updated["currency"], "USD");

    // Verify GET reflects the write (no PATCH-but-not-persisted bug).
    let after: Value = con
        .get("/api/admin/platform-pricing")
        .await
        .unwrap()
        .json()
        .unwrap();
    let after_input = after["input_price_per_token"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| {
            after["input_price_per_token"]
                .as_f64()
                .map(|f| f.to_string())
        })
        .unwrap();
    assert_eq!(
        Decimal::from_str(&after_input).unwrap(),
        Decimal::from_str(new_input).unwrap()
    );
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn platform_pricing_rejects_negative_values() {
    let app = TestApp::spawn().await;
    let con = admin_session(&app).await;

    let r1 = con
        .patch(
            "/api/admin/platform-pricing",
            json!({"input_price_per_token": "-0.0001"}),
        )
        .await
        .unwrap();
    assert_eq!(r1.status.as_u16(), 400, "negative input must 400");

    let r2 = con
        .patch(
            "/api/admin/platform-pricing",
            json!({"output_price_per_token": "-0.0001"}),
        )
        .await
        .unwrap();
    assert_eq!(r2.status.as_u16(), 400, "negative output must 400");
}

#[ignore = "integration test — run via `make test-it`"]
#[tokio::test]
async fn platform_pricing_patch_emits_audit_row() {
    let app = TestApp::spawn_with_clickhouse().await;
    let admin = fixtures::create_admin_user(&app.db).await.unwrap();
    let con = app.console_client();
    con.post(
        "/api/auth/login",
        json!({"email": admin.user.email, "password": admin.plaintext_password}),
    )
    .await
    .unwrap()
    .assert_ok();

    con.patch(
        "/api/admin/platform-pricing",
        json!({"input_price_per_token": "0.0000002500"}),
    )
    .await
    .unwrap()
    .assert_ok();

    let ch = app.state.clickhouse.as_ref().unwrap();
    let admin_id = admin.user.id.to_string();
    for _ in 0..200 {
        let row: Option<(String, String)> = ch
            .query(
                "SELECT action, ifNull(detail, '') FROM audit_logs \
                 WHERE user_id = ? AND action = 'platform_pricing.updated' \
                 ORDER BY created_at DESC LIMIT 1",
            )
            .bind(&admin_id)
            .fetch_optional()
            .await
            .unwrap();
        if let Some((action, detail)) = row {
            assert_eq!(action, "platform_pricing.updated");
            assert!(
                detail.contains("0.0000002500"),
                "audit detail should carry the new price, got {detail}"
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("platform_pricing.updated audit row never landed");
}
