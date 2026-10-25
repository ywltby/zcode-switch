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
