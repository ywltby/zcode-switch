use super::*;
use crate::test_support::SECRET;
use serde_json::json;
use std::fs;

fn temp_file() -> PathBuf {
    std::env::temp_dir()
        .join(format!("zcode-claim-session-{}", uuid::Uuid::new_v4()))
        .join("claim-attempts.json")
}

fn session(claim_id: &str, scope: &str, plan_id: &str) -> ClaimSession {
    ClaimSession {
        claim_id: claim_id.into(),
        account_id: "account-a".into(),
        account_name: "Synthetic A".into(),
        revision: "rev-1".into(),
        identity_scope: scope.into(),
        plan_id: plan_id.into(),
        plan_name: "Synthetic plan".into(),
        auto: false,
        created_at: now_ms(),
        updated_at: now_ms(),
        phase: ClaimPhase::Preparing,
        replaces: None,
        material: ClaimMaterial {
            credentials: json!({"zcodejwttoken": "synthetic-secret-token"}),
            config: None,
            device_mid: "synthetic-mid".into(),
        },
    }
}

fn verifying(registry: &mut ClaimRegistry, claim_id: &str, scope: &str, plan: &str) {
    registry.try_start(session(claim_id, scope, plan)).unwrap();
    assert!(registry.mark_verifying(claim_id));
}

fn expect_submit_error(result: Result<ClaimMaterial, SubmitError>, expected: SubmitError) {
    match result {
        Err(error) => assert_eq!(error, expected),
        Ok(_) => panic!("expected a refused submission"),
    }
}

#[test]
fn only_one_active_claim_is_accepted() {
    let mut registry = ClaimRegistry::load(temp_file());
    registry.try_start(session("c1", "zai:one", "p1")).unwrap();
    assert_eq!(
        registry.try_start(session("c2", "zai:two", "p1")).unwrap_err(),
        StartError::Busy
    );
    assert_eq!(registry.status("c1").unwrap().claim_id, "c1");
}

#[test]
fn same_claim_id_reentry_returns_the_existing_state() {
    let mut registry = ClaimRegistry::load(temp_file());
    registry.try_start(session("c1", "zai:one", "p1")).unwrap();
    let again = registry.try_start(session("c1", "zai:one", "p1")).unwrap();
    assert_eq!(again.claim_id, "c1");
    assert_eq!(again.status, "preparing");
    assert_eq!(registry.sessions().len(), 1);
}

#[test]
fn submit_permit_is_granted_once() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    assert!(registry.begin_submit("c1").is_ok());
    expect_submit_error(registry.begin_submit("c1"), SubmitError::NotReady);
}
#[test]
fn cancel_while_preparing_blocks_late_verification() {
    let mut registry = ClaimRegistry::load(temp_file());
    registry.try_start(session("c1", "zai:one", "p1")).unwrap();
    let outcome = registry.cancel("c1");
    assert!(outcome.cancelled);
    assert_eq!(outcome.phase, "cancelled");
    assert!(!registry.mark_verifying("c1"));
    assert_eq!(registry.status("c1").unwrap().status, "cancelled");
    registry.try_start(session("c2", "zai:two", "p1")).unwrap();
}

#[test]
fn cancel_while_submitting_keeps_the_mutex() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    let outcome = registry.cancel("c1");
    assert!(!outcome.cancelled);
    assert!(outcome.submission_in_flight);
    assert_eq!(registry.status("c1").unwrap().status, "submitting");
    assert_eq!(
        registry.try_start(session("c2", "zai:two", "p1")).unwrap_err(),
        StartError::Busy
    );
}

#[test]
fn foreign_claim_id_cannot_cancel_finish_or_submit() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    let outcome = registry.cancel("other");
    assert!(!outcome.cancelled);
    assert_eq!(outcome.phase, "verifying");
    assert!(!registry.finish("other", ClaimPhase::Succeeded, None));
    expect_submit_error(registry.begin_submit("other"), SubmitError::UnknownClaim);
    assert_eq!(registry.status("c1").unwrap().status, "verifying");
}

#[test]
fn worker_failure_releases_the_active_slot() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    assert!(registry.finish("c1", ClaimPhase::Failed, Some(json!({"code": 1005}))));
    assert_eq!(registry.status("c1").unwrap().status, "failed");
    registry.try_start(session("c2", "zai:two", "p1")).unwrap();
}

#[test]
fn finished_session_is_readable_and_finishing_twice_changes_nothing() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    assert!(registry.finish("c1", ClaimPhase::Succeeded, Some(json!({"endsAt": 10}))));
    assert!(!registry.finish("c1", ClaimPhase::Failed, None));
    let summary = registry.status("c1").unwrap();
    assert_eq!(summary.status, "succeeded");
    assert_eq!(summary.result, Some(json!({"endsAt": 10})));
    assert_eq!(registry.sessions().len(), 1);
}

#[test]
fn persist_failure_blocks_the_submission_and_keeps_verifying() {
    let path = temp_file();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // 先加载一个正常（不存在=空）的记录集，再把同名路径占成目录，
    // 让后续 persist 失败——这样才能单独验证「写入失败不发送 POST」。
    let mut registry = ClaimRegistry::load(path.clone());
    fs::create_dir_all(&path).unwrap();
    verifying(&mut registry, "c1", "zai:one", "p1");
    expect_submit_error(registry.begin_submit("c1"), SubmitError::PersistFailed);
    assert_eq!(registry.status("c1").unwrap().status, "verifying");
}

#[test]
fn restart_turns_a_submission_marker_into_unknown() {
    let path = temp_file();
    {
        let mut registry = ClaimRegistry::load(path.clone());
        verifying(&mut registry, "c1", "zai:one", "p1");
        registry.begin_submit("c1").unwrap();
        assert!(path.exists(), "the marker is written before the request");
    }
    let registry = ClaimRegistry::load(path.clone());
    let summary = registry.status("c1").unwrap();
    assert_eq!(summary.status, "unknown");
    assert!(registry.recovery_error().is_none());
    assert!(registry.blocks("zai:one", "p1"));
}

#[test]
fn unknown_blocks_same_identity_and_plan_only() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    registry.finish("c1", ClaimPhase::Unknown, None);
    assert!(registry.blocks("zai:one", "p1"));
    assert!(!registry.blocks("zai:one", "p2"));
    assert!(!registry.blocks("bigmodel:two", "p1"));
}

#[test]
fn only_an_acknowledged_manual_claim_passes_the_unconfirmed_gate() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    registry.finish("c1", ClaimPhase::Unknown, None);
    assert_eq!(registry.gate("auto", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.gate("all", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.gate("manual", "zai:one", "p1"), StartGate::Unconfirmed);
    assert!(registry.allow_manual_retry("c1"));
    assert_eq!(registry.gate("auto", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.gate("all", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(
        registry.gate("manual", "zai:one", "p1"),
        StartGate::Allowed(Some("c1".into()))
    );
    assert_eq!(registry.gate("manual", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.gate("manual", "zai:two", "p1"), StartGate::Allowed(None));
    assert_eq!(registry.gate("manual", "zai:one", "p2"), StartGate::Allowed(None));
}

#[test]
fn acknowledging_unknown_never_unblocks_automatic_claims() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    registry.finish("c1", ClaimPhase::Unknown, None);
    assert!(registry.blocks("zai:one", "p1"));
    assert!(registry.allow_manual_retry("c1"));
    assert!(registry.blocks("zai:one", "p1"), "an acknowledgement is not an automatic permit");
    assert_eq!(registry.consume_manual_retry("zai:one", "p1"), Some("c1".to_string()));
    assert_eq!(registry.consume_manual_retry("zai:one", "p1"), None, "the permit is one-shot");
    assert!(registry.blocks("zai:one", "p1"), "the unresolved record still blocks automatic claims");
    assert!(!registry.allow_manual_retry("missing"));
}

#[test]
fn a_definite_outcome_clears_the_unknown_it_replaced() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "old", "zai:one", "p1");
    registry.begin_submit("old").unwrap();
    registry.finish("old", ClaimPhase::Unknown, None);
    assert!(registry.allow_manual_retry("old"));

    let mut replacement = session("new", "zai:one", "p1");
    replacement.replaces = Some("old".into());
    registry.try_start(replacement).unwrap();
    assert!(registry.mark_verifying("new"));
    registry.begin_submit("new").unwrap();
    registry.finish("new", ClaimPhase::Succeeded, Some(json!({"ok": true})));
    assert!(!registry.blocks("zai:one", "p1"));
    assert!(registry.status("old").is_none(), "the superseded unknown is gone");
    assert_eq!(registry.status("new").unwrap().status, "succeeded");
}

#[test]
fn a_pre_submit_failure_keeps_the_unknown_it_replaced() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "old", "zai:one", "p1");
    registry.begin_submit("old").unwrap();
    registry.finish("old", ClaimPhase::Unknown, None);
    assert!(registry.allow_manual_retry("old"));
    let replaced = match registry.gate("manual", "zai:one", "p1") {
        StartGate::Allowed(replaced) => replaced,
        _ => panic!("explicit retry should be permitted"),
    };
    let mut retry = session("retry", "zai:one", "p1");
    retry.replaces = replaced;
    registry.try_start(retry).unwrap();
    // Preview/network failure before begin_submit: the original POST is still unknown.
    assert!(registry.finish("retry", ClaimPhase::Failed, None));
    assert!(
        registry.blocks("zai:one", "p1"),
        "a failed preview cannot establish the old POST outcome"
    );
    assert_eq!(registry.gate("auto", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.status("old").unwrap().status, "unknown");
}

fn retry_registry() -> (ClaimRegistry, String) {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "old", "zai:one", "p1");
    registry.begin_submit("old").unwrap();
    registry.finish("old", ClaimPhase::Unknown, None);
    assert!(registry.allow_manual_retry("old"));
    let replaced = match registry.gate("manual", "zai:one", "p1") {
        StartGate::Allowed(Some(replaced)) => replaced,
        _ => panic!("explicit retry should be permitted"),
    };
    (registry, replaced)
}

#[test]
fn a_local_auth_failure_after_the_marker_keeps_the_old_unknown() {
    let (mut registry, replaced) = retry_registry();
    let mut retry = session("retry", "zai:one", "p1");
    retry.replaces = Some(replaced);
    registry.try_start(retry).unwrap();
    assert!(registry.mark_verifying("retry"));
    registry.begin_submit("retry").unwrap();

    // Captured credentials can expire while the user solves the captcha. The
    // submission marker is not proof that the replacement POST was sent.
    let mut account = crate::test_support::account("review", "zai");
    account.credentials["zcodejwttoken"] = json!(crate::test_support::jwt(
        json!({"id": "zcode-review", "exp": 1})
    ));
    let error = crate::claim::submit_claim(
        &std::env::temp_dir(),
        &account.credentials,
        None,
        "p1",
        "synthetic-captcha",
        None,
        Some("synthetic-mid".into()),
    )
    .unwrap_err();
    assert!(!error.unknown, "the expired JWT fails before any POST");
    registry.finish("retry", ClaimPhase::Failed, Some(json!({"code": error.code})));
    assert!(
        registry.blocks("zai:one", "p1"),
        "a persisted marker is not proof that the replacement POST was sent"
    );
    assert_eq!(registry.gate("auto", "zai:one", "p1"), StartGate::Unconfirmed);
}

#[test]
fn a_captcha_rejection_of_the_retry_keeps_the_old_unknown() {
    let (mut registry, replaced) = retry_registry();
    let mut retry = session("retry", "zai:one", "p1");
    retry.replaces = Some(replaced);
    registry.try_start(retry).unwrap();
    assert!(registry.mark_verifying("retry"));
    registry.begin_submit("retry").unwrap();
    // 3007 rejects the NEW request; it says nothing about the OLD POST.
    registry.finish("retry", ClaimPhase::Failed, Some(json!({"code": 3007})));
    assert!(
        registry.blocks("zai:one", "p1"),
        "a new request's captcha rejection cannot resolve an old unknown result"
    );
    assert_eq!(registry.gate("auto", "zai:one", "p1"), StartGate::Unconfirmed);
    assert_eq!(registry.status("old").unwrap().status, "unknown");
}

#[test]
fn identity_scope_matches_a_uniquely_inferred_provider() {
    let explicit = crate::test_support::account("review", "zai");
    let mut inferred = explicit.credentials.clone();
    inferred
        .as_object_mut()
        .unwrap()
        .remove("oauth:active_provider");
    assert!(
        crate::quota_auth::strict_zcode_jwt(&inferred, SECRET).is_ok(),
        "the claim path infers the channel from unique credentials"
    );
    assert_eq!(
        identity_scope(&explicit.credentials, SECRET),
        identity_scope(&inferred, SECRET),
        "the same validated account must retain its unconfirmed-claim scope"
    );
}

#[test]
fn identity_scope_accepts_numeric_and_string_subjects_without_merging_accounts() {
    use crate::test_support::jwt;
    let with_id = |family: &str, id: serde_json::Value| {
        json!({
            "oauth:active_provider": family,
            "zcodejwttoken": jwt(json!({"id": id})),
        })
    };
    let numeric = identity_scope(&with_id("zai", json!(12345)), SECRET);
    let textual = identity_scope(&with_id("zai", json!("12345")), SECRET);
    let other = identity_scope(&with_id("zai", json!(12346)), SECRET);
    assert_eq!(numeric, textual, "the same subject must map to one scope");
    assert_ne!(numeric, other, "different numeric subjects must not share a scope");
    assert_ne!(numeric, identity_scope(&with_id("bigmodel", json!(12345)), SECRET));
}

#[test]
fn corrupt_attempts_are_reported_and_left_on_disk() {
    let path = temp_file();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "{not valid json").unwrap();
    let registry = ClaimRegistry::load(path.clone());
    assert_eq!(registry.recovery_error(), Some("claim_attempts_corrupt"));
    assert_eq!(fs::read_to_string(&path).unwrap(), "{not valid json");
    assert!(registry.sessions().is_empty());
}

#[test]
fn attempts_file_never_contains_credentials_or_captcha_parameters() {
    let path = temp_file();
    let mut registry = ClaimRegistry::load(path.clone());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    let raw = fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("synthetic-secret-token"), "no credential material");
    assert!(!raw.contains("captcha"), "no captcha parameter");
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["schemaVersion"], 1);
    let attempt = &parsed["attempts"][0];
    assert_eq!(attempt["claimId"], "c1");
    assert_eq!(attempt["planId"], "p1");
    assert_eq!(attempt["phase"], "submitting");
}

#[test]
fn persisted_entitlement_times_do_not_include_raw_claim_secrets() {
    let path = temp_file();
    let mut registry = ClaimRegistry::load(path.clone());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    let plan = json!({"entitlements":[{"show_name":"GLM-5.3-Flash","effective_at":1793113200,
        "token":"synthetic-private-token","captcha":"synthetic-private-captcha"}]});
    let times = crate::quota::entitlement_start_times(&plan);
    assert!(registry.finish("c1", ClaimPhase::Succeeded, Some(json!({"entitlementStarts":times}))));
    let raw = fs::read_to_string(&path).unwrap();
    assert!(!raw.contains("synthetic-private-token"));
    assert!(!raw.contains("synthetic-private-captcha"));
    let parsed: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(parsed["attempts"][0]["result"]["entitlementStarts"][0]["effective_at"], json!(1793113200000_i64));
}

/// 不可恢复的记录（schema 不支持 / phase 不认识 / 不可读）一律不覆盖原文件，
/// 并且拒绝任何新领取会话——自动、到点、批量和手动都经过 try_start。
#[test]
fn unrecoverable_attempts_are_never_rewritten_and_block_new_claims() {
    // 1) schema 不支持
    let path = temp_file();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let unsupported = r#"{"schemaVersion":99,"attempts":[{"claimId":"c9","accountId":"account-a","revision":"rev-1","scope":"zai:one","planId":"p1","phase":"unknown","updatedAt":1}]}"#;
    fs::write(&path, unsupported).unwrap();
    let mut registry = ClaimRegistry::load(path.clone());
    assert_eq!(
        registry.recovery_error(),
        Some("claim_attempts_unsupported")
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        unsupported,
        "schema 不支持的记录不得被重写"
    );
    assert_eq!(
        registry.try_start(session("c1", "zai:one", "p1")).unwrap_err(),
        StartError::Recovery
    );

    // 2) phase 不认识（可能是新版本写入的）
    let path = temp_file();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let unknown_phase = r#"{"schemaVersion":1,"attempts":[{"claimId":"c9","accountId":"account-a","revision":"rev-1","scope":"zai:one","planId":"p1","phase":"some-future-phase","updatedAt":1}]}"#;
    fs::write(&path, unknown_phase).unwrap();
    let mut registry = ClaimRegistry::load(path.clone());
    assert_eq!(registry.recovery_error(), Some("claim_attempts_corrupt"));
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        unknown_phase,
        "未知 phase 的记录不得被重写/丢弃"
    );
    assert_eq!(
        registry.try_start(session("c2", "zai:one", "p1")).unwrap_err(),
        StartError::Recovery
    );

    // 3) 不可读（用目录占位模拟读失败）
    let path = temp_file();
    fs::create_dir_all(&path).unwrap();
    let mut registry = ClaimRegistry::load(path.clone());
    assert!(
        registry
            .recovery_error()
            .is_some_and(|e| e.starts_with("claim_attempts_unreadable")),
        "读取失败必须报告: {:?}",
        registry.recovery_error()
    );
    assert!(path.is_dir(), "不可读的文件不得被删除或替换成空记录");
    assert_eq!(
        registry.try_start(session("c3", "zai:one", "p1")).unwrap_err(),
        StartError::Recovery
    );
}
