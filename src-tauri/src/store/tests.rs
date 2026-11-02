use super::*;
use crate::quota_http::Request;
use crate::test_support::{account, quota_response, SECRET};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    paths: Paths,
}
impl Fixture {
    fn new() -> Self {
        let home = std::env::temp_dir().join(format!("zcode-quota-test-{}", Uuid::new_v4()));
        fs::create_dir_all(home.join(".zcode/v2")).unwrap();
        let paths = Paths {
            home: home.clone(),
            data_root: home,
        };
        save_account(&paths, &account("a", "zai")).unwrap();
        fs::write(
            paths.live_file(),
            serde_json::to_vec(&account("b", "bigmodel").credentials).unwrap(),
        )
        .unwrap();
        fs::write(paths.live_telemetry(), r#"{"deviceMid":"live-b-mid"}"#).unwrap();
        fs::write(paths.settings_file(), r#"{"auto_claim":false}"#).unwrap();
        Self { paths }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let (Ok(temp), Ok(home)) = (fs::canonicalize(std::env::temp_dir()), fs::canonicalize(&self.paths.home)) {
            if home.parent() == Some(temp.as_path())
                && home.file_name().is_some_and(|name| name.to_string_lossy().starts_with("zcode-quota-test-"))
            {
                let _ = fs::remove_dir_all(home);
            }
        }
    }
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut result = BTreeMap::new();
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            result.extend(contents(&path));
        } else {
            result.insert(path.clone(), fs::read(path).unwrap());
        }
    }
    result
}

fn expect_scope_changed(result: Result<ClaimSnapshot, String>) {
    match result {
        Err(error) => assert_eq!(error, "scope_changed"),
        Ok(_) => panic!("expected a scope change"),
    }
}

#[test]
fn claim_snapshot_initializes_the_device_once_and_rejects_the_old_revision() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let mut fresh = account("c", "zai");
    fresh.virtual_device_mid = None;
    save_account(paths, &fresh).unwrap();
    let secret = zcrypto::default_secret(&paths.home);
    let before = crate::quota_auth::quota_revision(&fresh, &secret);

    let first = claim_snapshot_locked(paths, "account-c", Some(&before)).unwrap();
    assert!(first.device_initialized);
    assert_ne!(first.revision, before, "the device id changes the revision");
    let mid = first.account.virtual_device_mid.clone().unwrap();
    assert!(!mid.trim().is_empty());

    // The same revision is now stale: no activation, preview, or claim may start.
    expect_scope_changed(claim_snapshot_locked(paths, "account-c", Some(&before)));

    let second = claim_snapshot_locked(paths, "account-c", Some(&first.revision)).unwrap();
    assert!(!second.device_initialized, "the device id is initialized once");
    assert_eq!(second.account.virtual_device_mid.as_deref(), Some(mid.as_str()));
    assert_eq!(second.revision, first.revision);
}

#[test]
fn claim_revision_check_detects_updates_and_deletion() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let snapshot = claim_snapshot_locked(paths, "account-a", None).unwrap();
    assert!(claim_revision_matches(paths, "account-a", &snapshot.revision));

    let mut updated = load_account(paths, "account-a").unwrap();
    updated.credentials["zcodejwttoken"] = serde_json::json!(crate::test_support::jwt(serde_json::json!({"id": "replacement"})));
    save_account(paths, &updated).unwrap();
    assert!(!claim_revision_matches(paths, "account-a", &snapshot.revision));

    delete_account(paths, "account-a").unwrap();
    assert!(!claim_revision_matches(paths, "account-a", &snapshot.revision));
    expect_scope_changed(claim_snapshot_locked(paths, "account-a", Some(&snapshot.revision)));
}

#[test]
fn hot_switch_preserves_native_device_state_and_saved_claim_device() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let telemetry_before = fs::read(paths.live_telemetry()).unwrap();
    let target = read_quota_account(paths, "account-a").unwrap();

    // Simulate the process check only; every file operation stays in the fixture.
    let switched = switch_to_with_process_check(paths, "account-a", false, false, true, || true).unwrap();
    assert!(switched.switched && switched.hot);
    assert_eq!(fs::read(paths.live_telemetry()).unwrap(), telemetry_before);
    assert_eq!(read_live(paths).unwrap().unwrap(), target.credentials);
    assert_eq!(read_quota_account(paths, "account-a").unwrap().virtual_device_mid, target.virtual_device_mid);
}

#[test]
fn expired_target_is_rejected_before_process_check_or_file_changes() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let mut target = read_quota_account(paths, "account-a").unwrap();
    target.credentials["zcodejwttoken"] = json!(crate::test_support::jwt(json!({"id":"zcode-a", "exp":1})));
    save_account(paths, &target).unwrap();
    let before = contents(&paths.home);

    let result = switch_to_with_process_check(paths, "account-a", true, false, true, || {
        panic!("an expired target must not reach process handling")
    });
    assert!(result.is_err());
    assert_eq!(contents(&paths.home), before);
}

#[test]
fn adding_an_existing_account_updates_its_expired_login_but_preserves_claim_device() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let mut old = account("renew", "bigmodel");
    old.credentials["zcodejwttoken"] = json!(crate::test_support::jwt(json!({"id":"zcode-renew", "exp":1})));
    save_account(paths, &old).unwrap();
    let fresh_jwt = crate::test_support::jwt(json!({"id":"zcode-renew", "exp":4102444800_i64}));
    let raw = json!({"data":{"token":fresh_jwt, "user":{"user_id":"zcode-renew"}}});
    *crate::pending_oauth_guard() = Some(crate::PendingOAuth {
        provider: "bigmodel".into(), state: "synthetic-state".into(), flow: "synthetic-flow".into(),
    });

    let result = crate::persist_oauth_account(paths, "bigmodel", &raw, "synthetic-flow", "new-oauth-mid", true).unwrap();
    assert_eq!(result["duplicate"], true);
    let updated = read_quota_account(paths, "account-renew").unwrap();
    assert_eq!(updated.credentials["zcodejwttoken"], fresh_jwt);
    assert_eq!(updated.virtual_device_mid, old.virtual_device_mid);
    assert_eq!(list_accounts(paths).unwrap().len(), 2);
}

#[test]
fn account_query_is_read_only_and_independent_of_live_account() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let before = contents(&paths.home);
    let result = query_snapshot_via(
        read_quota_account(paths, "account-a").unwrap(),
        SECRET,
        "3.16.3",
        &|req: Request| {
            assert!(!req.headers.iter().any(|(_, v)| v.contains("live-b-mid")));
            Ok(quota_response(&req))
        },
        &crate::project_token::TokenService::default(),
        || read_quota_account(paths, "account-a"),
    )
    .unwrap();
    assert_eq!(result.account_id, "account-a");
    assert_eq!(result.completion, "complete");
    assert_eq!(before, contents(&paths.home));
}

#[test]
fn query_stops_after_account_update_or_delete_during_request() {
    for delete in [false, true] {
        let fixture = Fixture::new();
        let paths = &fixture.paths;
        let calls = AtomicUsize::new(0);
        let result = query_snapshot_via(
            read_quota_account(paths, "account-a").unwrap(),
            SECRET,
            "3.16.3",
            &|req: Request| {
                assert_eq!(
                    calls.fetch_add(1, Ordering::SeqCst),
                    0,
                    "old scope continued sending requests"
                );
                if delete {
                    delete_account(paths, "account-a").unwrap();
                } else {
                    let mut a = read_quota_account(paths, "account-a").unwrap();
                    a.credentials = account("c", "zai").credentials;
                    save_account(paths, &a).unwrap();
                }
                Ok(quota_response(&req))
            },
            &crate::project_token::TokenService::default(),
            || read_quota_account(paths, "account-a"),
        );
        assert!(matches!(result,Err(e) if e=="scope_changed"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn live_query_discards_response_after_live_login_changes() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let calls = AtomicUsize::new(0);
    let result = query_snapshot_via(
        read_live_quota_snapshot(paths).unwrap(),
        SECRET,
        "3.16.3",
        &|req: Request| {
            assert_eq!(calls.fetch_add(1, Ordering::SeqCst), 0);
            fs::write(
                paths.live_file(),
                serde_json::to_vec(&account("c", "zai").credentials).unwrap(),
            )
            .unwrap();
            Ok(quota_response(&req))
        },
        &crate::project_token::TokenService::default(),
        || read_live_quota_snapshot(paths),
    );
    assert!(matches!(result,Err(e) if e=="scope_changed"));
}

#[test]
fn legacy_bundle_import_needs_no_new_fields_and_preserves_existing_device() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let existing = read_quota_account(paths, "account-a").unwrap();
    let live_before = fs::read(paths.live_file()).unwrap();
    let mut old = account("c", "zai");
    old.credentials["oauth:zai:user_info"] = json!(json!({"user_id":"zcode-c"}).to_string());
    let bundle = json!({"format":"zcode-accounts-bundle", "version":1, "accounts":[
        {"name":"Synthetic legacy C", "credentials":old.credentials}
    ]});
    let report = import_values(paths, &[("synthetic-old.zsb".into(), bundle)]).unwrap();
    assert_eq!(report.added, ["Synthetic legacy C"]);
    assert!(report.errors.is_empty());
    let imported = list_accounts(paths)
        .unwrap()
        .into_iter()
        .find(|a| a.name == "Synthetic legacy C")
        .unwrap();
    assert_eq!(imported.credentials, old.credentials);
    assert!(imported.config.is_none());
    let ctx = crate::quota_auth::resolve_account_context(&imported, SECRET, "3.16.3").unwrap();
    assert_eq!(ctx.scope.zcode_subject, "zcode-c");
    assert_eq!(
        read_quota_account(paths, "account-a")
            .unwrap()
            .virtual_device_mid,
        existing.virtual_device_mid
    );
    assert_eq!(fs::read(paths.live_file()).unwrap(), live_before);
}

#[test]
fn unknown_identity_never_sends_network_requests_and_old_snapshots_still_read() {
    let fixture = Fixture::new();
    let paths = &fixture.paths;
    let mut a = account("a", "zai");
    a.credentials = serde_json::json!({"legacy":"unknown"});
    let mut json = serde_json::to_value(a).unwrap();
    json.as_object_mut().unwrap().remove("virtual_device_mid");
    json.as_object_mut().unwrap().remove("config");
    fs::write(
        paths.accounts_dir().join("account-a.json"),
        json.to_string(),
    )
    .unwrap();
    let old = read_quota_account(paths, "account-a").unwrap();
    assert!(old.virtual_device_mid.is_none());
    let result = query_snapshot_via(
        old,
        SECRET,
        "3.16.3",
        &|_: Request| panic!("unknown identity must not query"),
        &crate::project_token::TokenService::default(),
        || read_quota_account(paths, "account-a"),
    )
    .unwrap();
    assert_eq!(result.completion, "unavailable");
    assert_eq!(
        result.errors[0].code,
        crate::quota_auth::QuotaError::IdentityMissing
    );
}
