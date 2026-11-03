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

/// 图形界面的「当前登录」必须走 live 快照：读 live 凭据与 telemetry 设备 id，
/// 不查 accounts/live.json，也不创建或改写客户端设备文件。
#[test]
fn live_claim_snapshot_uses_live_credentials_and_telemetry_mid() {
    let f = Fixture::new();
    let telemetry_before = fs::read(f.paths.live_telemetry()).unwrap();
    let snapshot = claim_snapshot_locked(&f.paths, LIVE_ID, None).unwrap();
    assert_eq!(snapshot.account.id, LIVE_ID);
    assert_eq!(snapshot.account.credentials, account("b", "bigmodel").credentials);
    assert_eq!(
        snapshot.account.virtual_device_mid.as_deref(),
        Some("live-b-mid")
    );
    assert!(!snapshot.device_initialized, "live 设备 id 不由本工具初始化");
    assert!(
        !f.paths.accounts_dir().join("live.json").exists(),
        "live 快照不得依赖 accounts/live.json"
    );
    assert_eq!(
        fs::read(f.paths.live_telemetry()).unwrap(),
        telemetry_before,
        "live 快照不得改写 telemetry 设备文件"
    );
}

/// 换登录后旧 revision 立刻失效：迟到的查询/领取结果不能结算到新登录。
#[test]
fn live_revision_matches_tracks_the_live_login() {
    let f = Fixture::new();
    let revision = claim_snapshot_locked(&f.paths, LIVE_ID, None)
        .unwrap()
        .revision;
    assert!(claim_revision_matches(&f.paths, LIVE_ID, &revision));
    expect_scope_changed(claim_snapshot_locked(&f.paths, LIVE_ID, Some("stale-revision")));

    fs::write(
        f.paths.live_file(),
        serde_json::to_vec(&account("c", "zai").credentials).unwrap(),
    )
    .unwrap();
    assert!(
        !claim_revision_matches(&f.paths, LIVE_ID, &revision),
        "换登录后旧 revision 必须失效"
    );
    expect_scope_changed(claim_snapshot_locked(
        &f.paths,
        LIVE_ID,
        Some(&revision),
    ));
}

/// 领取历史按真实登录身份隔离：同一个 live 行换登录后不得看到上一个登录的记录。
#[test]
fn claim_history_is_scoped_to_the_live_identity() {
    let f = Fixture::new();
    record_claim_history(&f.paths, LIVE_ID, "zai:user-one", "p1", "Plan One");
    record_claim_history(&f.paths, LIVE_ID, "zai:user-two", "p2", "Plan Two");
    record_claim_history(&f.paths, "account-a", "zai:account-a", "p3", "Plan Three");

    let one = claim_history_for_identity(&f.paths, LIVE_ID, Some("zai:user-one"));
    assert_eq!(one.len(), 1, "只返回同一身份的记录");
    assert_eq!(one[0]["planId"], "p1");
    assert_eq!(one[0]["identity"], "zai:user-one");

    let two = claim_history_for_identity(&f.paths, LIVE_ID, Some("zai:user-two"));
    assert_eq!(two.len(), 1);
    assert_eq!(two[0]["planId"], "p2");

    assert!(
        claim_history_for_identity(&f.paths, LIVE_ID, Some("zai:user-three")).is_empty(),
        "新登录不得看到旧登录历史"
    );
    // 普通账号与「不过滤」调用保持旧行为
    assert_eq!(claim_history_for(&f.paths, "account-a").len(), 1);
    assert_eq!(claim_history_for(&f.paths, LIVE_ID).len(), 2);
}

/// 没有 identity 字段的旧记录在按身份过滤时不得混入（重启后仍安全）。
#[test]
fn legacy_history_without_identity_is_not_matched_by_identity() {
    let f = Fixture::new();
    let file = claim_history_file(&f.paths);
    fs::write(
        &file,
        r#"[{"accountId":"live","planId":"old","planName":"Old","claimedAt":1}]"#,
    )
    .unwrap();
    assert!(
        claim_history_for_identity(&f.paths, LIVE_ID, Some("zai:user-one")).is_empty(),
        "旧的无身份记录不能算到当前登录头上"
    );
    assert_eq!(claim_history_for(&f.paths, LIVE_ID).len(), 1, "不过滤时仍可见");
}

/// 界面身份必须携带后端作用域：领取标记、活动目录与缓存都用它分区，
/// 因此不能只靠 user_id / provider 自行拼接，也不能用固定 "live"。
#[test]
fn ui_identity_carries_the_backend_scope() {
    let f = Fixture::new();
    let state = get_state(&f.paths).unwrap();
    let secret = zcrypto::default_secret(&f.paths.home);

    let live = state.live_identity.clone().expect("live 身份应存在");
    let live_expected = crate::claim_session::identity_scope(
        &account("b", "bigmodel").credentials,
        &secret,
    );
    assert_eq!(
        live.scope.as_deref(),
        Some(live_expected.as_str()),
        "live 作用域必须与 claim 会话使用的 identity_scope 一致"
    );
    assert_eq!(live.provider, "bigmodel");

    let stored = state
        .accounts
        .iter()
        .find(|a| a.id == "account-a")
        .unwrap_or_else(|| panic!("账号 account-a 应存在"));
    let stored_expected =
        crate::claim_session::identity_scope(&account("a", "zai").credentials, &secret);
    assert_eq!(stored.identity.scope.as_deref(), Some(stored_expected.as_str()));

    // 同一个人在不同渠道 / 不同主体不能合并成同一个作用域
    assert_ne!(
        live.scope, stored.identity.scope,
        "渠道与主体不同必须得到不同作用域"
    );
}

/// 无身份的旧历史不能冒充当前登录的记录：单独归类，只有用户显式清理才删除。
#[test]
fn unattributed_history_is_kept_apart_and_can_be_dropped() {
    let f = Fixture::new();
    let file = claim_history_file(&f.paths);
    fs::write(
        &file,
        r#"[
            {"accountId":"account-a","identity":"zai:user-one","planId":"p1","planName":"New","claimedAt":10},
            {"accountId":"account-a","planId":"legacy-1","planName":"Old","claimedAt":5},
            {"accountId":"account-b","planId":"other","planName":"Other","claimedAt":6}
        ]"#,
    )
    .unwrap();

    let legacy = claim_history_legacy_for(&f.paths, "account-a");
    assert_eq!(legacy.len(), 1, "只有无 identity 的条目算作未归属");
    assert_eq!(legacy[0]["planId"], "legacy-1");

    // 当前身份的读取不受影响
    let current = claim_history_for_identity(&f.paths, "account-a", Some("zai:user-one"));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["planId"], "p1");

    let dropped = drop_claim_history_legacy(&f.paths, "account-a").expect("清除应成功");
    assert_eq!(dropped, 1);
    assert!(claim_history_legacy_for(&f.paths, "account-a").is_empty());
    assert_eq!(
        claim_history_for_identity(&f.paths, "account-a", Some("zai:user-one")).len(),
        1,
        "清理旧记录不得影响当前身份的历史"
    );
    // 其它账号的条目保持原样
    assert_eq!(claim_history_for(&f.paths, "account-b").len(), 1);

    // 清除的读写失败必须返回错误（不能悄悄当成功）
    let broken = Fixture::new();
    let broken_file = claim_history_file(&broken.paths);
    fs::create_dir_all(&broken_file).unwrap();
    assert!(
        drop_claim_history_legacy(&broken.paths, "account-a").is_err(),
        "读不出来 / 写不进去都必须报错"
    );

    // 损坏的 JSON 也必须报错，而不是当作「没有旧记录」
    let corrupt = Fixture::new();
    fs::write(claim_history_file(&corrupt.paths), "{not json").unwrap();
    assert!(claim_history_legacy_check(&corrupt.paths, "account-a").is_err());
    assert!(drop_claim_history_legacy(&corrupt.paths, "account-a").is_err());
}

/// 旧历史归属：给无 identity 的条目补上当前身份作用域；失败必须报错（前端不能放开自动领取）。
#[test]
fn legacy_history_can_be_adopted_only_with_a_real_write() {
    let f = Fixture::new();
    let file = claim_history_file(&f.paths);
    fs::write(
        &file,
        r#"[
            {"accountId":"account-a","planId":"legacy-1","planName":"Old","claimedAt":5},
            {"accountId":"account-a","identity":"zai:user-one","planId":"p1","planName":"New","claimedAt":10},
            {"accountId":"account-b","planId":"other","planName":"Other","claimedAt":6}
        ]"#,
    )
    .unwrap();

    let adopted = adopt_claim_history_legacy(&f.paths, "account-a", "zai:user-two")
        .expect("归属写入应成功");
    assert_eq!(adopted.plan_ids, vec!["legacy-1".to_string()]);
    assert_eq!(adopted.skipped, 0);
    assert!(claim_history_legacy_for(&f.paths, "account-a").is_empty());
    let current = claim_history_for_identity(&f.paths, "account-a", Some("zai:user-two"));
    assert_eq!(current.len(), 1);
    assert_eq!(current[0]["planId"], "legacy-1");
    // 已有身份的条目不被改写；其它账号保持原样
    assert_eq!(
        claim_history_for_identity(&f.paths, "account-a", Some("zai:user-one")).len(),
        1
    );
    assert_eq!(claim_history_for(&f.paths, "account-b").len(), 1);

    // 无可归属条目时返回空结果（幂等），不报错
    let again = adopt_claim_history_legacy(&f.paths, "account-a", "zai:user-two").unwrap();
    assert!(again.plan_ids.is_empty());
    assert_eq!(again.skipped, 0);

    // 没有套餐 ID 的旧记录无法可靠归属：保持未归属并计入 skipped
    let unkeyed = Fixture::new();
    fs::write(
        claim_history_file(&unkeyed.paths),
        r#"[{"accountId":"account-a","planName":"No id","claimedAt":7}]"#,
    )
    .unwrap();
    let outcome = adopt_claim_history_legacy(&unkeyed.paths, "account-a", "zai:user-x").unwrap();
    assert!(outcome.plan_ids.is_empty());
    assert_eq!(outcome.skipped, 1);
    assert_eq!(
        claim_history_legacy_for(&unkeyed.paths, "account-a").len(),
        1,
        "无可识别套餐 ID 的记录不得被静默丢弃"
    );

    // 写失败（历史文件被目录占位）必须报错，调用方据此保留旧状态
    let broken = Fixture::new();
    let broken_file = claim_history_file(&broken.paths);
    fs::create_dir_all(&broken_file).unwrap();
    assert!(adopt_claim_history_legacy(&broken.paths, "account-a", "zai:user-two").is_err());
}
