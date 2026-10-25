use super::*;
use crate::test_support::{account, jwt, SECRET};
use serde_json::json;

#[test]
fn accepts_normalized_and_raw_profiles_without_comparing_business_subject() {
    for field in ["id", "user_id"] {
        let mut a = account("a", "zai");
        a.credentials["oauth:zai:user_info"] = json!(json!({field: "zcode-a"}).to_string());
        let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
        assert_eq!(ctx.scope.zcode_subject, "zcode-a");
        assert_eq!(ctx.auth.business_subject.as_deref(), Some("business-a"));
        assert_eq!(ctx.scope.snapshot_id, "account-a");
    }
}

#[test]
fn encrypted_current_login_pair_resolves_without_using_historical_credentials() {
    use aes_gcm::aead::{Aead, KeyInit};
    use aes_gcm::{Aes256Gcm, Nonce};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use sha2::{Digest, Sha256};
    let mut a = account("a", "zai");
    let plain = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    let cipher = Aes256Gcm::new_from_slice(&Sha256::digest(SECRET.as_bytes())).unwrap();
    for (index, key) in [
        "zcodejwttoken",
        "oauth:zai:access_token",
        "oauth:zai:user_info",
    ]
    .into_iter()
    .enumerate()
    {
        // Deterministic nonces are only for this synthetic fixture and synthetic secret.
        let nonce = [index as u8; 12];
        let sealed = cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                a.credentials[key].as_str().unwrap().as_bytes(),
            )
            .unwrap();
        let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
        a.credentials[key] = json!(format!(
            "enc:v1:{}.{}.{}",
            URL_SAFE_NO_PAD.encode(nonce),
            URL_SAFE_NO_PAD.encode(tag),
            URL_SAFE_NO_PAD.encode(ciphertext)
        ));
    }
    let decoded = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    assert_eq!(decoded.scope.zcode_subject, plain.scope.zcode_subject);
    assert_eq!(decoded.start_token().unwrap(), plain.start_token().unwrap());
    assert_eq!(
        decoded.business_token().unwrap(),
        plain.business_token().unwrap()
    );
    assert_eq!(decoded.auth.business_subject, plain.auth.business_subject);
    assert_eq!(
        strict_zcode_jwt(&a.credentials, SECRET).unwrap(),
        plain.start_token().unwrap()
    );
    assert!(matches!(
        resolve_account_context(&a, "wrong-synthetic-secret", "3.16.3"),
        Err(QuotaError::LoginRequired)
    ));
}

#[test]
fn normalizes_numeric_ids_and_rejects_conflicts() {
    let mut a = account("a", "zai");
    a.credentials["oauth:zai:user_info"] = json!(json!({"id": 123}).to_string());
    a.credentials["zcodejwttoken"] = json!(jwt(json!({"user_id":"123"})));
    assert!(resolve_account_context(&a, SECRET, "3.16.3").is_ok());
    a.credentials["zcodejwttoken"] = json!(jwt(json!({"user_id":"456"})));
    assert!(matches!(
        resolve_account_context(&a, SECRET, "3.16.3"),
        Err(QuotaError::IdentityConflict)
    ));
}

#[test]
fn missing_provider_requires_unique_current_login() {
    let mut a = account("a", "zai");
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("oauth:active_provider");
    assert_eq!(
        resolve_account_context(&a, SECRET, "3.16.3")
            .unwrap()
            .scope
            .family,
        ProviderFamily::Zai
    );
    a.credentials["oauth:bigmodel:access_token"] = json!("another-login");
    a.credentials["oauth:bigmodel:user_info"] = json!(json!({"id":"another"}).to_string());
    assert!(matches!(
        resolve_account_context(&a, SECRET, "3.16.3"),
        Err(QuotaError::IdentityMissing)
    ));
}

#[test]
fn missing_exp_is_allowed_but_expired_jwt_and_bad_ciphertext_fail() {
    let mut a = account("a", "zai");
    assert!(resolve_account_context(&a, SECRET, "3.16.3").is_ok());
    a.credentials["zcodejwttoken"] = json!(jwt(json!({"id":"zcode-a", "exp":1})));
    let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    assert_eq!(ctx.start_token(), Err(QuotaError::LoginRequired));
    a.credentials["zcodejwttoken"] = json!("enc:v1:corrupt");
    assert!(matches!(
        resolve_account_context(&a, SECRET, "3.16.3"),
        Err(QuotaError::LoginRequired)
    ));
}

#[test]
fn revision_ignores_history_and_names_but_tracks_login_and_device() {
    let mut a = account("a", "zai");
    let original = resolve_account_context(&a, SECRET, "3.16.3")
        .unwrap()
        .scope
        .revision;
    a.name = "renamed".into();
    a.credentials["account-provider:foreign:coding-plan:api-key"] = json!("other-history");
    assert_eq!(
        resolve_account_context(&a, SECRET, "3.16.3")
            .unwrap()
            .scope
            .revision,
        original
    );
    a.virtual_device_mid = Some("another-mid".into());
    assert_ne!(
        resolve_account_context(&a, SECRET, "3.16.3")
            .unwrap()
            .scope
            .revision,
        original
    );
}

#[test]
fn missing_identity_does_not_guess_from_business_token() {
    let mut a = account("a", "zai");
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("oauth:zai:user_info");
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("zcodejwttoken");
    assert!(matches!(
        resolve_account_context(&a, SECRET, "3.16.3"),
        Err(QuotaError::IdentityMissing)
    ));
}

#[test]
fn legacy_profile_display_identity_uses_zcode_id_not_business_id() {
    let mut a = account("a", "zai");
    a.credentials["oauth:zai:user_info"] =
        json!(json!({"user_id":"zcode-a","name":"Old profile"}).to_string());
    let id = crate::zcrypto::identity_with_secret(&a.credentials, SECRET);
    assert_eq!(id.user_id.as_deref(), Some("zcode-a"));
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("oauth:active_provider");
    let id = crate::zcrypto::identity_with_secret(&a.credentials, SECRET);
    assert_eq!(id.provider, "zai");
}

#[test]
fn damaged_business_token_does_not_block_own_start_jwt_or_profile() {
    let mut a = account("a", "zai");
    a.credentials["oauth:zai:access_token"] = json!("enc:v1:corrupt");
    let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    assert!(ctx.start_token().is_ok());
    assert_eq!(ctx.business_token(), Err(QuotaError::LoginRequired));
    assert!(strict_zcode_jwt(&a.credentials, SECRET).is_ok());
    assert_eq!(
        crate::zcrypto::identity_with_secret(&a.credentials, SECRET)
            .user_id
            .as_deref(),
        Some("zcode-a")
    );
}
