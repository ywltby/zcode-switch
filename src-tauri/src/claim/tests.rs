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
