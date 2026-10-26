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
    fs::create_dir_all(&path).unwrap();
    let mut registry = ClaimRegistry::load(path);
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
