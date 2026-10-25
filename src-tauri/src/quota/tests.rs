use super::*;
use crate::quota_auth::resolve_account_context;
use crate::quota_auth::{PlanScope, QuotaError};
use crate::quota_http::Request;
use crate::test_support::{account, SECRET};
use serde_json::json;
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
