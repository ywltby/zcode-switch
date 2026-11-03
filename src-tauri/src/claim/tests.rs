//! 领取凭据与错误载荷的隔离回归：恢复自合并前基线（8a5e6b9 曾删除本文件）。
//!
//! 原基线另有三个 `CaptchaCache` 单元测试（endpoint 隔离 / TTL 有界 / transient 重试一次）。
//! 验证码配置实现随合并改为「单 endpoint + 600s TTL + 失败回退上次成功值再退内置默认」，
//! 对应保护由 `tests/mock_upstream.rs::captcha_config_uses_mock_then_falls_back_to_default`
//! 继续断言（缓存命中不重发请求、失败不阻塞领取流程）。

use super::*;
use crate::test_support::{account, jwt, SECRET};
use serde_json::json;

#[test]
fn claim_never_uses_foreign_config_key_or_conflicting_jwt() {
    let mut a = account("a", "bigmodel");
    assert_eq!(
        claim_token(&a.credentials, a.config.as_ref(), SECRET).unwrap(),
        a.credentials["zcodejwttoken"].as_str().unwrap()
    );
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("zcodejwttoken");
    assert!(claim_token(&a.credentials, a.config.as_ref(), SECRET).is_err());
    a.credentials["zcodejwttoken"] = json!(jwt(json!({"id":"foreign"})));
    assert!(claim_token(&a.credentials, a.config.as_ref(), SECRET).is_err());
}

#[test]
fn scope_payload_is_structured_and_carries_no_credentials() {
    let payload: Value = serde_json::from_str(&scope_payload("account-a", "rev-1", true)).unwrap();
    assert_eq!(payload["code"], "scope_changed");
    assert_eq!(payload["accountId"], "account-a");
    assert_eq!(payload["revision"], "rev-1");
    assert_eq!(payload["notStarted"], true);
    assert!(payload.get("credentials").is_none());
    let after_submit: Value = serde_json::from_str(&scope_payload("account-a", "rev-1", false)).unwrap();
    assert_eq!(after_submit["notStarted"], false);
}

#[test]
fn claim_display_name_falls_back_to_the_plan_id() {
    let plans = Ok(vec![ClaimPlan {
        plan_id: "p1".into(),
        name: String::new(),
        description: String::new(),
        priority: 0,
        grants: vec![],
        grant_items: vec![],
        campaign: None,
    }]);
    assert_eq!(claim_display_name(plans, "p1"), "p1");
    assert_eq!(
        claim_display_name(Err("preview failed".into()), "p9"),
        "p9",
        "a failed eligibility read must not block the claim"
    );
    assert_eq!(
        claim_display_name(
            Ok(vec![ClaimPlan {
                plan_id: "p2".into(),
                name: "Named".into(),
                description: String::new(),
                priority: 0,
                grants: vec![],
                grant_items: vec![],
                campaign: None,
            }]),
            "p2"
        ),
        "Named"
    );
}

#[test]
fn unknown_claim_errors_are_never_terminal_or_retryable() {
    let unknown = unknown_claim_error("transport died mid-flight".into());
    assert!(unknown.unknown);
    assert!(!unknown.is_terminal(), "an unknown result is not a rejection");
    assert!(!unknown.is_captcha_rejected());
    let payload = failure_payload("a", "A", "Plan", &unknown);
    assert_eq!(payload["unknown"], true);
    assert_eq!(payload["terminal"], false);
}

/// Tauri 结果的字段完整性：terminal / challenge / nextAt / unknown 必须全部到达
/// 事件载荷（自动队列与到点领取据此决定是否重试）。
#[test]
fn failure_payload_keeps_every_field_the_claim_queue_needs() {
    let quota = ClaimError {
        code: 1005,
        message: "quota".into(),
        next_at: Some(1_700_000_100_000),
        unknown: false,
    };
    let payload = failure_payload("account-a", "A", "Plan", &quota);
    assert_eq!(payload["code"], 1005);
    assert_eq!(payload["terminal"], true);
    assert_eq!(payload["challenge"], false);
    assert_eq!(payload["unknown"], false);
    assert_eq!(payload["nextAt"], 1_700_000_100_000_i64);

    let terminal = ClaimError {
        code: 1002,
        message: "ended".into(),
        next_at: None,
        unknown: false,
    };
    let payload = failure_payload("account-a", "A", "Plan", &terminal);
    assert_eq!(payload["terminal"], true);
    assert_eq!(payload["nextAt"], Value::Null);

    let challenge = ClaimError {
        code: CLAIM_ERR_CHALLENGE,
        message: "challenge".into(),
        next_at: None,
        unknown: false,
    };
    let payload = failure_payload("account-a", "A", "Plan", &challenge);
    assert_eq!(payload["challenge"], true);
    assert_eq!(payload["terminal"], false, "人机校验不是终态");

    let rejected = ClaimError {
        code: 3007,
        message: "captcha rejected".into(),
        next_at: None,
        unknown: false,
    };
    let payload = failure_payload("account-a", "A", "Plan", &rejected);
    assert_eq!(payload["challenge"], true, "3007 是验证码拒绝");
    assert_eq!(payload["terminal"], false);

    let risk = ClaimError {
        code: 3012,
        message: "3012 unusual activity".into(),
        next_at: None,
        unknown: true,
    };
    let payload = failure_payload("account-a", "A", "Plan", &risk);
    assert_eq!(payload["code"], 3012);
    assert_eq!(payload["terminal"], false);
    assert_eq!(payload["challenge"], false);
    assert_eq!(payload["unknown"], true);

    let lost = unknown_claim_error("lost".into());
    let payload = failure_payload("account-a", "A", "Plan", &lost);
    assert_eq!(payload["unknown"], true);
    assert_eq!(payload["terminal"], false);
    assert_eq!(payload["nextAt"], Value::Null);
}
