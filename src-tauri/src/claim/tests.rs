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

fn captcha_body(enabled: bool, scene: &str, code: i64) -> Value {
    json!({"code": code, "data": {"configs": {"captcha": {
        "enabled": enabled, "region": "cn", "prefix": "synthetic", "sceneId": scene
    }}}})
}

#[test]
fn captcha_config_cache_is_endpoint_scoped_and_ttl_bounded() {
    use std::cell::Cell;
    let cache = CaptchaCache::new();
    let calls = Cell::new(0);
    let http = |_: &str| {
        calls.set(calls.get() + 1);
        Ok((200, captcha_body(true, "scene-1", 0)))
    };
    let first = cache.fetch("https://config-a", &http, 1_000).unwrap();
    assert_eq!(first.scene_id, "scene-1");
    assert_eq!(calls.get(), 1);

    let cached = cache
        .fetch("https://config-a", &http, 1_000 + CAPTCHA_CONFIG_TTL_MS - 1)
        .unwrap();
    assert_eq!(cached.scene_id, "scene-1");
    assert_eq!(calls.get(), 1, "a usable config is served from the cache");

    cache.fetch("https://config-b", &http, 2_000).unwrap();
    assert_eq!(calls.get(), 2, "the cache is keyed by endpoint");

    cache
        .fetch("https://config-a", &http, 1_000 + CAPTCHA_CONFIG_TTL_MS + 1)
        .unwrap();
    assert_eq!(calls.get(), 3, "expired entries are fetched again");
}

#[test]
fn captcha_config_retries_transient_failures_once() {
    use std::cell::Cell;
    let cache = CaptchaCache::new();
    let calls = Cell::new(0);
    let flaky = |_: &str| {
        calls.set(calls.get() + 1);
        if calls.get() == 1 {
            Err(CaptchaFetchError::Transient)
        } else {
            Ok((200, captcha_body(true, "scene", 0)))
        }
    };
    assert!(cache.fetch("https://config", &flaky, 0).is_ok());
    assert_eq!(calls.get(), 2);

    let failing = CaptchaCache::new();
    let attempts = Cell::new(0);
    let broken = |_: &str| {
        attempts.set(attempts.get() + 1);
        Err(CaptchaFetchError::Transient)
    };
    assert!(failing.fetch("https://config", &broken, 0).is_err());
    assert_eq!(attempts.get(), 2, "at most two GETs for transient failures");
}

#[test]
fn captcha_config_does_not_retry_rejections_and_never_caches_unusable_configs() {
    use std::cell::Cell;
    for status in [401_u16, 429] {
        let cache = CaptchaCache::new();
        let calls = Cell::new(0);
        let denied = |_: &str| {
            calls.set(calls.get() + 1);
            Ok((status, Value::Null))
        };
        assert!(cache.fetch("https://config", &denied, 0).is_err());
        assert_eq!(calls.get(), 1, "HTTP {status} is not retried");
    }
    let cache = CaptchaCache::new();
    let calls = Cell::new(0);
    let business_error = |_: &str| {
        calls.set(calls.get() + 1);
        Ok((200, captcha_body(true, "scene", 3002)))
    };
    assert!(cache.fetch("https://config", &business_error, 0).is_err());
    assert_eq!(calls.get(), 1, "business rejections are not retried");

    let disabled_calls = Cell::new(0);
    let disabled = |_: &str| {
        disabled_calls.set(disabled_calls.get() + 1);
        Ok((200, captcha_body(false, "", 0)))
    };
    let config = cache.fetch("https://disabled", &disabled, 0).unwrap();
    assert!(!config.enabled);
    assert_eq!(disabled_calls.get(), 1);
    cache.fetch("https://disabled", &disabled, 1).unwrap();
    assert_eq!(disabled_calls.get(), 2, "an unusable config is not cached");
}
