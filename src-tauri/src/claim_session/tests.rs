use super::*;
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
fn acknowledging_unknown_allows_an_explicit_manual_retry() {
    let mut registry = ClaimRegistry::load(temp_file());
    verifying(&mut registry, "c1", "zai:one", "p1");
    registry.begin_submit("c1").unwrap();
    registry.finish("c1", ClaimPhase::Unknown, None);
    assert!(registry.acknowledge_unknown("c1"));
    assert!(!registry.blocks("zai:one", "p1"));
    assert_eq!(registry.status("c1").unwrap().status, "unknown");
    assert!(!registry.acknowledge_unknown("missing"));
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
