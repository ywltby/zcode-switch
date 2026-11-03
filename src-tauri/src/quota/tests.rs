use super::*;
use crate::quota_auth::resolve_account_context;
use crate::quota_auth::{PlanScope, QuotaError};
use crate::quota_http::Request;
use crate::test_support::{account, SECRET};
use serde_json::json;
use std::fs;
use std::sync::Mutex;

#[test]
fn rejects_foreign_credentials() {
    let a = account("a", "zai");
    let trace = Mutex::new(Vec::new());
    let own_jwt = a.credentials["zcodejwttoken"].as_str().unwrap();
    let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    let _result = query_context_via(&ctx, &|request: Request| {
        trace.lock().unwrap().push(
            request
                .headers
                .iter()
                .find(|(key, _)| key == "Authorization")
                .unwrap()
                .1
                .clone(),
        );
        Ok(crate::test_support::quota_response(&request))
    });
    assert!(!trace.lock().unwrap().is_empty());
    let allowed = [
        format!("Bearer {own_jwt}"),
        format!("Bearer {}", ctx.business_token().unwrap()),
        ctx.business_token().unwrap().into(),
        "synthetic-own-project-token".into(),
    ];
    assert!(
        trace
            .lock()
            .unwrap()
            .iter()
            .all(|token| allowed.contains(token)),
        "request used a foreign credential"
    );
}

#[test]
fn bigmodel_uses_own_jwt_and_never_falls_back_on_auth_failure() {
    let b = account("b", "bigmodel");
    let ctx = resolve_account_context(&b, SECRET, "3.16.3").unwrap();
    let calls = Mutex::new(0);
    query_context_via(&ctx, &|request: Request| {
        *calls.lock().unwrap() += 1;
        let expected = if request.url.contains("billing/balance") {
            format!("Bearer {}", ctx.auth.zcode_jwt.as_ref().unwrap())
        } else {
            ctx.business_token().unwrap().to_string()
        };
        assert!(request
            .headers
            .iter()
            .any(|(k, v)| k == "Authorization" && v == &expected));
        Err(crate::quota_auth::QuotaError::LoginRequired)
    });
    assert_eq!(*calls.lock().unwrap(), 2); // Own Start JWT and own business login only.
}

#[test]
fn device_and_version_come_from_each_immutable_account() {
    for label in ["a", "b", "a"] {
        let a = account(label, "zai");
        let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
        let calls = Mutex::new(0);
        query_context_via(&ctx, &|request: Request| {
            if request.url.contains("billing/balance") {
                *calls.lock().unwrap() += 1;
                assert!(request
                    .headers
                    .contains(&("X-Device-Mid".into(), format!("synthetic-mid-{label}"))));
                assert!(request
                    .headers
                    .contains(&("X-ZCode-App-Version".into(), "3.16.3".into())));
            }
            Ok(crate::test_support::quota_response(&request))
        });
        assert_eq!(*calls.lock().unwrap(), 1);
    }
}

fn scoped(
    ctx: &AccountQueryContext,
    project: &str,
    instance: Option<&str>,
    period: &str,
    used: Option<f64>,
    time: i64,
) -> ScopedPart {
    ScopedPart {
        account: ctx.scope.clone(),
        plan: PlanScope::Personal {
            organization_id: "org".into(),
            project_id: project.into(),
        },
        value: QuotaOverview {
            plans: vec![PlanSlot {
                pid: "same-product".into(),
                entitlement_id: instance.map(String::from),
                period_key: Some(period.into()),
                tier: Some("Pro".into()),
                name: Some("Same plan".into()),
                items: used
                    .map(|u| {
                        vec![QuotaItem {
                            window_key: Some("daily-window".into()),
                            total: Some(100.0),
                            used: Some(u),
                            ..Default::default()
                        }]
                    })
                    .unwrap_or_default(),
                ..Default::default()
            }],
            refreshed_at: time,
            ..Default::default()
        },
    }
}

#[test]
fn merge_rejects_foreign_account_even_with_identical_product_id() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let other = resolve_account_context(&account("c", "zai"), SECRET, "3.16.3").unwrap();
    let result = merge_scoped(
        &ctx,
        vec![scoped(&other, "p", Some("id"), "period", Some(1.0), 1)],
        vec![],
    );
    assert_eq!(result.completion, "unavailable");
    assert!(result.plans.is_empty());
    assert_eq!(result.errors[0].code, QuotaError::ScopeChanged);
}

#[test]
fn merge_preserves_projects_periods_and_unproven_duplicates() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let result = merge_scoped(
        &ctx,
        vec![
            scoped(&ctx, "p1", Some("id"), "oct", Some(1.0), 1),
            scoped(&ctx, "p2", Some("id"), "oct", Some(1.0), 1),
            scoped(&ctx, "p1", Some("id"), "nov", Some(1.0), 1),
            scoped(&ctx, "p1", None, "oct", Some(1.0), 1),
            scoped(&ctx, "p1", None, "oct", Some(1.0), 1),
        ],
        vec![],
    );
    assert_eq!(result.plans.len(), 5);
}

#[test]
fn merge_prefers_completeness_then_freshness_and_never_sums_duplicate_windows() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let result = merge_scoped(
        &ctx,
        vec![
            scoped(&ctx, "p", Some("id"), "oct", None, 5),
            scoped(&ctx, "p", Some("id"), "oct", Some(10.0), 2),
            scoped(&ctx, "p", Some("id"), "oct", Some(20.0), 3),
        ],
        vec![],
    );
    assert_eq!(result.plans.len(), 1);
    assert_eq!(result.items.len(), 1);
    assert_eq!(result.items[0].used, Some(20.0));
}

#[test]
fn merge_partial_and_all_failed_do_not_mean_no_plan() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let err = || ScopeError {
        scope: "Coding Plan".into(),
        code: QuotaError::NetworkError,
    };
    let result = merge_scoped(
        &ctx,
        vec![scoped(&ctx, "p", Some("id"), "oct", Some(10.0), 1)],
        vec![err()],
    );
    assert_eq!(result.completion, "partial");
    assert!(!result.is_empty);
    let failed = merge_scoped(&ctx, vec![], vec![err()]);
    assert_eq!(failed.completion, "unavailable");
    assert!(!failed.is_empty);
    let empty = ScopedPart {
        account: ctx.scope.clone(),
        plan: PlanScope::Start,
        value: QuotaOverview::default(),
    };
    let confirmed = merge_scoped(&ctx, vec![empty], vec![]);
    assert_eq!(confirmed.completion, "complete");
    assert!(confirmed.is_empty);
}

#[test]
fn balance_same_product_different_periods_attach_to_their_entitlement() {
    let result = normalize_balance(&json!({"code":0,"data":{
        "server_time":1790000000,
        "plans":[
            {"plan_id":"start","subscription_id":"oct","status":"active","starts_at":1790800000,"ends_at":1793500000},
            {"plan_id":"start","subscription_id":"nov","status":"active","starts_at":1793500000,"ends_at":1796100000}
        ],
        "balances":[
            {"plan_id":"start","subscription_id":"oct","name":"credits","total_units":100,"used_units":10,"period_end":"2026-10-31"},
            {"plan_id":"start","subscription_id":"nov","name":"credits","total_units":100,"used_units":20,"period_end":"2026-11-30"}
        ]
    }}));
    assert_eq!(result.plans.len(), 2);
    assert_eq!(result.plans[0].items.len(), 1);
    assert_eq!(result.plans[1].items.len(), 1);
    assert_eq!(result.plans[1].items[0].used, Some(20.0));
}

#[test]
fn balance_preserves_each_plan_start_time_for_display() {
    let result = normalize_balance(&json!({"code":0,"data":{
        "server_time":1800000000,
        "plans":[
            {"plan_id":"start","subscription_id":"current","status":"active","starts_at":1799990000,"ends_at":1900000000},
            {"plan_id":"start","subscription_id":"next","status":"active","starts_at":1800086400,"ends_at":1900000000}
        ],
        "balances":[]
    }}));
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["plans"][0]["starts_at"], json!(1799990000000_i64));
    assert_eq!(json["plans"][1]["starts_at"], json!(1800086400000_i64));
}

#[test]
fn start_plan_uses_each_entitlement_effective_time_instead_of_claim_time() {
    let result = normalize_balance(&json!({"code":0,"data":{
        "server_time":1793111645,
        "plans":[{"plan_id":"start","status":"active","starts_at":1793099353,"ends_at":1793149200,
            "entitlements":[
                {"entitlement_id":"first","show_name":"GLM-5.3-Flash","effective_at":1793113200},
                {"entitlement_id":"second","show_name":"Other model","effective_at":1793116800000_i64},
                {"entitlement_id":"third","show_name":"Unknown time"}
            ]},
            {"plan_id":"start","subscription_id":"empty","status":"active","starts_at":1793099353,"ends_at":1793149200}
        ],"balances":[]
    }}));
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["plans"][0]["starts_at"], json!(1793099353000_i64), "plan time is retained as metadata");
    assert_eq!(json["plans"][0]["entitlement_starts"], json!([
        {"name":"GLM-5.3-Flash","effective_at":1793113200000_i64},
        {"name":"Other model","effective_at":1793116800000_i64},
        {"name":"Unknown time","effective_at":null}
    ]));
    assert_eq!(json["plans"][1]["entitlement_starts"], json!([]), "do not infer a missing activation time");
}

#[test]
fn merged_plan_uses_the_latest_entitlement_schedule_even_if_older_quota_is_more_complete() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let make = |effective: i64, balances: Value, at: i64| {
        let mut value = normalize_balance(&json!({"code":0,"data":{"server_time":1793111645,
            "plans":[{"plan_id":"start","subscription_id":"same","status":"active",
                "starts_at":1793099353,"ends_at":1793149200,
                "entitlements":[{"show_name":"GLM","effective_at":effective}]}],"balances":balances}}));
        value.refreshed_at = at;
        ScopedPart { account:ctx.scope.clone(), plan:PlanScope::Start, value }
    };
    let older = make(1793113200, json!([{"plan_id":"start","subscription_id":"same",
        "name":"credits","total_units":100,"used_units":20,"remaining_units":80}]), 100);
    let newer = make(1793116800, json!([]), 200);
    let result = serde_json::to_value(merge_scoped(&ctx, vec![older, newer], vec![])).unwrap();
    assert_eq!(result["plans"][0]["entitlement_starts"][0]["effective_at"], json!(1793116800000_i64));
    assert_eq!(result["plans"][0]["items"].as_array().unwrap().len(), 1);

    let older = make(1793113200, json!([]), 100);
    let mut missing = normalize_balance(&json!({"code":0,"data":{"server_time":1793111645,
        "plans":[{"plan_id":"start","subscription_id":"same","status":"active",
            "starts_at":1793099353,"ends_at":1793149200}],"balances":[]}}));
    missing.refreshed_at = 200;
    let newer = ScopedPart {account:ctx.scope.clone(), plan:PlanScope::Start, value:missing};
    let result = serde_json::to_value(merge_scoped(&ctx, vec![older, newer], vec![])).unwrap();
    assert_eq!(result["plans"][0]["entitlement_starts"], json!([]), "do not reuse an obsolete start time");
}

#[test]
fn start_time_supports_seconds_milliseconds_and_iso_without_guessing_invalid_values() {
    for (value, expected) in [
        (json!(1800086400_i64), json!(1800086400000_i64)),
        (json!(1800086400000_i64), json!(1800086400000_i64)),
        (json!("1800086400"), json!(1800086400000_i64)),
        (json!("2030-01-02T01:00:00Z"), json!(1893546000000_i64)),
        (json!("not-a-date"), serde_json::Value::Null),
        (serde_json::Value::Null, serde_json::Value::Null),
    ] {
        let result = normalize_balance(&json!({"code":0,"data":{
            "server_time":1800000000,
            "plans":[{"plan_id":"start","status":"active","starts_at":value,"ends_at":2000000000}],
            "balances":[]
        }}));
        assert_eq!(serde_json::to_value(&result).unwrap()["plans"][0]["starts_at"], expected);
    }
}

#[test]
fn start_time_survives_coding_plan_and_team_fallback() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    for missing_team_key in [false, true] {
        let result = query_context_via(&ctx, &|req: Request| {
            if missing_team_key && req.url.ends_with("api_keys") && req.url.contains("team-") {
                return Ok(json!({"code":0,"data":[]}));
            }
            let mut response = crate::test_support::quota_response(&req);
            if req.url.ends_with("querySubscribeDetail") {
                response["data"]["subscribeStartTime"] = json!(1800086400000_i64);
            }
            if req.url.ends_with("subscription/list") {
                response["data"][0]["subscribeStartTime"] = json!(1800086400_i64);
            }
            Ok(response)
        });
        let json = serde_json::to_value(&result).unwrap();
        let plans = json["plans"].as_array().unwrap();
        let coding: Vec<_> = plans.iter().filter(|p| p["tier_code"] == "pro").collect();
        assert_eq!(coding.len(), 3);
        assert!(coding.iter().all(|p| p["starts_at"] == json!(1800086400000_i64)));
    }
}

#[test]
fn coding_plan_valid_range_preserves_both_local_times() {
    use chrono::{NaiveDateTime, TimeZone};
    let sub = json!({"productId":"coding-pro","productName":"Coding Pro",
        "status":"VALID","inCurrentPeriod":true,
        "valid":"2030-01-02 09:30:15-2030-01-05 12:00:00"});
    let expected = |text: &str| chrono::Local.from_local_datetime(
        &NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()).single().unwrap().timestamp_millis();
    let slot = subscription_slot(&sub);
    assert_eq!(slot.starts_at, Some(expected("2030-01-02 09:30:15")));
    assert_eq!(slot.ends_at, Some(expected("2030-01-05 12:00:00")));
    let q = normalize_quota_limit(&json!({"code":0,"data":{"level":"Pro","limits":[]}}),
        Some(&json!({"code":0,"data":[sub]})));
    assert_eq!(q.plans[0].starts_at, Some(expected("2030-01-02 09:30:15")));
    assert_eq!(q.plans[0].ends_at, Some(expected("2030-01-05 12:00:00")));
    assert_eq!(subscription_slot(&json!({"valid":"2030-01-05"})).starts_at, None,
        "an end date on its own is not a start time");
}

#[test]
fn personal_subscriptions_keep_valid_noncurrent_plans_until_they_expire() {
    let now = chrono::Local::now();
    let range = |start: chrono::DateTime<chrono::Local>, end: chrono::DateTime<chrono::Local>| {
        format!("{}-{}", start.format("%Y-%m-%d %H:%M:%S"), end.format("%Y-%m-%d %H:%M:%S"))
    };
    let valid_future = range(now + chrono::Duration::days(1), now + chrono::Duration::days(3));
    let valid_waiting = range(now - chrono::Duration::hours(1), now + chrono::Duration::days(2));
    let expired = range(now - chrono::Duration::days(3), now - chrono::Duration::days(1));
    let expired_day = (now - chrono::Duration::days(2)).format("%Y-%m-%d").to_string();
    let raw = json!({"code":0,"data":[
        {"productId":"coding-current","status":"VALID","inCurrentPeriod":true},
        {"productId":"coding-next","status":"VALID","inCurrentPeriod":false,"valid":valid_future},
        {"productId":"coding-waiting","status":"VALID","inCurrentPeriod":false,"valid":valid_waiting},
        {"productId":"coding-unknown-end","status":"VALID","inCurrentPeriod":false},
        {"productId":"coding-expired","status":"VALID","inCurrentPeriod":false,"valid":expired},
        {"productId":"coding-expired-day","status":"VALID","inCurrentPeriod":false,"valid":expired_day},
        {"productId":"coding-cancelled","status":"CANCELLED","inCurrentPeriod":false,"valid":valid_future}
    ]});
    let (kept, malformed) = personal_subscriptions(&raw).unwrap();
    assert!(!malformed);
    assert_eq!(kept.len(), 4);
    assert_eq!(kept[1]["productId"], "coding-next");
    assert_eq!(kept[2]["productId"], "coding-waiting");
    assert_eq!(kept[3]["productId"], "coding-unknown-end");
    assert_eq!(serde_json::to_value(subscription_slot(&kept[1])).unwrap()["pending"], true);
    assert_eq!(serde_json::to_value(subscription_slot(&kept[2])).unwrap()["pending"], true);
}

#[test]
fn full_query_keeps_the_future_coding_plan_without_copying_current_usage_into_it() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let now = chrono::Local::now();
    let valid = format!("{}-{}", (now + chrono::Duration::days(1)).format("%Y-%m-%d %H:%M:%S"),
        (now + chrono::Duration::days(3)).format("%Y-%m-%d %H:%M:%S"));
    let result = query_context_via(&ctx, &|req: Request| {
        let mut response = crate::test_support::quota_response(&req);
        if req.url.ends_with("subscription/list") {
            response["data"].as_array_mut().unwrap().push(json!({
                "productId":"coding-next", "productName":"Coding Pro next",
                "subscriptionId":"next-sub", "status":"VALID", "inCurrentPeriod":false, "valid":valid
            }));
        }
        Ok(response)
    });
    assert_eq!(result.completion, "complete");
    let pending = result.plans.iter().find(|p| p.entitlement_id.as_deref() == Some("next-sub")).unwrap();
    assert!(pending.pending);
    assert!(pending.starts_at.is_some() && pending.ends_at.is_some());
    assert!(pending.items.is_empty(), "current project usage must not be duplicated into a future plan");
}

#[test]
fn expired_plans_are_hidden_but_future_plans_survive_mixed_time_units() {
    for (now, expired_end, current_end, future_end) in [
        (json!(1800000000_i64), json!(1799999999000_i64), json!(1800100000000_i64), json!(1800200000000_i64)),
        (json!(1800000000000_i64), json!(1799999999_i64), json!(1800100000_i64), json!(1800200000_i64)),
    ] {
        let result = normalize_balance(&json!({"code":0,"data":{
            "server_time":now,
            "plans":[
                {"plan_id":"start","subscription_id":"expired","status":"active","starts_at":1790000000,"ends_at":expired_end},
                {"plan_id":"start","subscription_id":"current","status":"active","starts_at":1790000000,"ends_at":current_end},
                {"plan_id":"start","subscription_id":"future","status":"active","starts_at":1800050000,"ends_at":future_end}
            ],
            "balances":[]
        }}));
        assert_eq!(result.plans.len(), 2);
        assert_eq!(result.plans[0].entitlement_id.as_deref(), Some("current"));
        assert_eq!(result.plans[1].entitlement_id.as_deref(), Some("future"));
        assert_eq!(serde_json::to_value(&result).unwrap()["plans"][1]["ends_at"], json!(1800200000000_i64));
    }
}

#[test]
fn full_query_keeps_two_team_projects_and_uses_scoped_project_tokens() {
    let a = account("a", "zai");
    let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    let calls = Mutex::new(Vec::new());
    let result = query_context_via(&ctx, &|req: Request| {
        let auth = req
            .headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap()
            .1
            .clone();
        let allowed = [
            format!("Bearer {}", ctx.auth.zcode_jwt.as_ref().unwrap()),
            format!("Bearer {}", ctx.business_token().unwrap()),
            ctx.business_token().unwrap().to_string(),
            "synthetic-own-project-token".into(),
        ];
        assert!(allowed.contains(&auth), "foreign credential used");
        let response = crate::test_support::quota_response(&req);
        calls
            .lock()
            .unwrap()
            .push((req.method, req.url, req.headers));
        Ok(response)
    });
    assert_eq!(result.completion, "complete");
    assert_eq!(result.plans.len(), 4);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _, _)| *m == "POST")
            .count(),
        3
    );
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, url, _)| url.ends_with("type=2"))
            .count(),
        2
    );
}

#[test]
fn legacy_key_scene_distinguishes_active_and_absent_coding_plan() {
    for family in ["zai", "bigmodel"] {
        for subscribed in [false, true] {
            let ctx = resolve_account_context(&account("a", family), SECRET, "3.16.3").unwrap();
            let quota_calls = Mutex::new(0);
            let result = query_context_via(&ctx, &|req: Request| {
                let mut response = crate::test_support::quota_response(&req);
                if req.url.contains("billing/balance") {
                    response = json!({"code":0,"data":{"plans":[],"balances":[]}});
                } else if req.url.ends_with("/api_keys") {
                    for key in response["data"].as_array_mut().unwrap() {
                        key["usageScene"] = json!(-1);
                    }
                } else if req.url.ends_with("querySubscribeDetail") {
                    response = json!({"code":0,"data":{"hasSubscription":false}});
                } else if req.url.ends_with("subscription/list") && !subscribed {
                    response = json!({"code":0,"data":[]});
                } else if req.url.contains("quota/limit") {
                    *quota_calls.lock().unwrap() += 1;
                }
                Ok(response)
            });
            assert_eq!(result.completion, "complete");
            assert!(result.errors.is_empty());
            assert_eq!(result.is_empty, !subscribed);
            assert_eq!(result.plans.len(), usize::from(subscribed));
            assert_eq!(*quota_calls.lock().unwrap(), usize::from(subscribed));
        }
    }
}

#[test]
fn full_query_distinguishes_confirmed_empty_from_unknown_subscription() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    for malformed in [false, true] {
        let result = query_context_via(&ctx, &|req: Request| {
            Ok(if req.url.contains("billing/balance") {
                json!({"code":0,"data":{"plans":[],"balances":[]}})
            } else if req.url.ends_with("subscription/list") {
                if malformed {
                    json!({"code":0,"data":[{"productId":"coding-pro","status":"FUTURE"}]})
                } else {
                    json!({"code":0,"data":[]})
                }
            } else if req.url.ends_with("querySubscribeDetail") {
                json!({"code":0,"data":{"hasSubscription":false}})
            } else {
                crate::test_support::quota_response(&req)
            })
        });
        assert_eq!(result.is_empty, !malformed);
        assert_eq!(
            result.completion,
            if malformed { "partial" } else { "complete" }
        );
    }
}

#[test]
fn balance_official_user_plan_and_bucket_contract() {
    let raw = json!({"code":0,"data":{"server_time":1900000000,
    "plans":[
        {"plan_id":"start","user_plan_id":"current","status":"active","starts_at":1890000000,"ends_at":1910000000,"entitlements":[{"entitlement_id":"model-credits","period":"daily"}]},
        {"plan_id":"start","user_plan_id":"expired","status":"active","starts_at":1880000000,"ends_at":1890000000}
    ],
    "balances":[
        {"plan_id":"start","user_plan_id":"current","bucket_id":"today","entitlement_id":"model-credits","show_name":"GLM-5.3","total_units":100,"used_units":20,"remaining_units":80,"available_units":70,"period_start":1900000000,"period_end":1900086400,"expires_at":1900086400},
        {"plan_id":"start","user_plan_id":"expired","bucket_id":"old","entitlement_id":"model-credits","total_units":100,"used_units":1,"expires_at":1890000000}
    ]}});
    let result = normalize_balance(&raw);
    assert_eq!(
        result.plans.len(),
        1,
        "expired instance and unowned duplicate must not appear"
    );
    assert_eq!(result.plans[0].entitlement_id.as_deref(), Some("current"));
    assert_eq!(result.plans[0].items.len(), 1);
    assert_eq!(result.plans[0].remaining, Some(80.0));
    assert_eq!(result.plans[0].expire, expiry_field(&json!(1910000000)));
    assert!(result.plans[0].items[0]
        .window_key
        .as_ref()
        .unwrap()
        .contains("today"));
}

#[test]
fn unknown_start_status_is_not_a_confirmed_empty_plan() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let result = query_context_via(&ctx, &|req: Request| {
        Ok(if req.url.contains("billing/balance") {
            json!({"code":0,"data":{"plans":[{"plan_id":"start","status":"NEW_UNKNOWN"}],"balances":[]}})
        } else if req.url.ends_with("subscription/list") {
            json!({"code":0,"data":[]})
        } else if req.url.ends_with("querySubscribeDetail") {
            json!({"code":0,"data":{"hasSubscription":false}})
        } else {
            crate::test_support::quota_response(&req)
        })
    });
    assert_eq!(result.completion, "partial");
    assert!(!result.is_empty);
    assert!(result
        .errors
        .iter()
        .any(|e| e.code == QuotaError::InvalidResponse));
}

#[test]
fn team_subscription_survives_missing_project_key() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let result = query_context_via(&ctx, &|req: Request| {
        Ok(
            if req.url.ends_with("api_keys") && req.url.contains("team-") {
                json!({"code":0,"data":[]})
            } else {
                crate::test_support::quota_response(&req)
            },
        )
    });
    assert_eq!(result.completion, "partial");
    assert_eq!(
        result.plans.len(),
        4,
        "retain both confirmed team subscriptions"
    );
    assert_eq!(
        result
            .errors
            .iter()
            .filter(|e| e.code == QuotaError::ProjectKeyMissing)
            .count(),
        2
    );
}

#[test]
fn balance_status_whitespace_matches_validation() {
    let result = normalize_balance(&json!({"code":0,"data":{"server_time":1900000000,"plans":[
        {"user_plan_id":"active","plan_id":"start","status":" active ","ends_at":1910000000}
    ],"balances":[]}}));
    assert_eq!(result.plans.len(), 1);
}

#[test]
fn version_comparison_is_numeric_not_lexicographic() {
    assert!(version_ge("3.16.3", "3.15.1"));
    assert!(version_ge("3.16.3", "3.16.3"));
    assert!(!version_ge("3.9", "3.10"), "3.9 必须小于 3.10");
    assert!(version_ge("3.16", "3.16.0"), "缺失段按 0 计");
    assert!(!version_ge("2.99", "3.0"));
}

#[test]
fn claim_version_never_drops_below_the_builtin_floor() {
    assert_eq!(pick_app_version("3.1.0"), CLAIM_APP_VERSION);
    assert_eq!(pick_app_version("3.16.3"), CLAIM_APP_VERSION);
    assert_eq!(pick_app_version("3.17.2"), "3.17.2");
    assert_eq!(normalize_version("3.17.2.5"), "3.17.2");
}

#[test]
fn app_version_keeps_the_freshest_known_value() {
    // 注入 home，不触碰进程环境变量与全局缓存：并行执行也不会污染其他用例。
    let home = std::env::temp_dir().join(format!(
        "zsw-version-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(home.join(".zcode-switch")).unwrap();
    let cache = home.join(".zcode-switch").join("changelog-cache.json");

    // 较新的 changelog 缓存生效
    fs::write(&cache, r#"{"version":"9.9.9","fetched_at":9999999999}"#).unwrap();
    assert_eq!(app_version_for(&home), "9.9.9");

    // 旧缓存不得把版本降级到内置下限以下
    fs::write(&cache, r#"{"version":"3.0.0","fetched_at":9999999999}"#).unwrap();
    let fresh = app_version_for(&home);
    assert!(
        version_ge(&fresh, CLAIM_APP_VERSION),
        "旧缓存/探测失败时必须回退到内置下限，实际 {fresh}"
    );

    // 无缓存时同样不低于内置下限
    let _ = fs::remove_file(&cache);
    assert!(version_ge(&app_version_for(&home), CLAIM_APP_VERSION));

    // 两个 home 互不影响（无共享缓存）
    let other = home.join("other");
    fs::create_dir_all(other.join(".zcode-switch")).unwrap();
    fs::write(
        other.join(".zcode-switch").join("changelog-cache.json"),
        r#"{"version":"9.9.10","fetched_at":9999999999}"#,
    )
    .unwrap();
    assert_eq!(app_version_for(&other), "9.9.10");
    assert!(version_ge(&app_version_for(&home), CLAIM_APP_VERSION));
    assert!(!version_ge(&app_version_for(&home), "9.9.10"));

    let _ = fs::remove_dir_all(&home);
}
