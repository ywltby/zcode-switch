pub mod cipher;
pub mod cli;
pub mod i18n;
pub mod changelog;
pub mod claim;
mod claim_session;
mod flowlog;
mod net;
mod oauth;
mod probe;
mod project_token;
mod quota;
mod quota_auth;
mod quota_http;
mod risk;
mod store;
#[cfg(test)]
mod test_support;
mod zcrypto;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_prepare_aborts_on_risk_signals_only() {
        // 3012 / unusual activity：必须中止（不打开验证码、不提交）
        assert!(claim_prepare_abort(
            r#"preview HTTP 405: {"provider_code":3012,"message":"request has been blocked due to unusual activity"}"#
        )
        .is_some());
        assert!(claim_prepare_abort("blocked due to unusual activity").is_some());
        assert!(claim_prepare_abort("命中风控策略").is_some());
        // 普通预览失败：降级继续（beta.11：本地列表不决定最终资格）
        assert!(claim_prepare_abort("preview network_error").is_none());
        assert!(claim_prepare_abort("project_key_missing").is_none());
        assert!(claim_prepare_abort("").is_none());
    }
}

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use store::*;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::AutoLaunchManager;
use tauri_plugin_dialog::DialogExt;

const TRAY_ID: &str = "main";

static STORE_LOCK: Mutex<()> = Mutex::new(());

/// Process-wide egress proxy. This is the single slot every outbound agent reads
/// (quota, claim/preview/marketing, activation reporting and probes); the GUI's
/// `set_auth_proxy` and the CLI both write it.
pub fn set_traffic_proxy(url: Option<String>) {
    net::set_proxy(url);
}

pub fn traffic_proxy() -> Option<String> {
    net::proxy()
}

/// 托盘 tooltip 上附加的「新活动」提示（None = 无提示）。
/// Windows 便携版拿不到 Toast，悬停托盘图标看提示是最可靠的可见通道。
static ACTIVITY_TIP: Mutex<Option<String>> = Mutex::new(None);
/// 托盘闪烁开关：true 时后台线程持续交替图标，直到用户点开活动详情。
static TRAY_FLASH_ACTIVE: AtomicBool = AtomicBool::new(false);

fn activity_tip_guard() -> std::sync::MutexGuard<'static, Option<String>> {
    match ACTIVITY_TIP.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn claim_registry_guard() -> std::sync::MutexGuard<'static, Option<claim_session::ClaimRegistry>> {
    static CLAIM_REGISTRY: std::sync::Mutex<Option<claim_session::ClaimRegistry>> =
        std::sync::Mutex::new(None);
    match CLAIM_REGISTRY.lock() {
        Ok(mut guard) => {
            if guard.is_none() {
                let paths = Paths::detect();
                *guard = Some(claim_session::ClaimRegistry::load(
                    claim_session::attempts_path(&paths.store_dir()),
                ));
            }
            guard
        }
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Claim snapshots cover a stored account (which owns a virtual device id) and the
/// login ZCode runs right now (`live`, real live credentials + telemetry device id,
/// nothing written to disk).
fn claim_snapshot(paths: &Paths, id: &str, expected: Option<&str>) -> Result<store::ClaimSnapshot, String> {
    let mut snapshot = store::claim_snapshot_locked(paths, id, expected)?;
    if id == store::LIVE_ID {
        snapshot.account.name = zcrypto::account_identity(&snapshot.account.credentials, &paths.home)
            .label()
            .unwrap_or_else(|| i18n::tr("claim.live_name"));
    }
    Ok(snapshot)
}

/// Re-reading must not revive a claim prepared for a different login.
fn claim_revision_ok(paths: &Paths, id: &str, revision: &str) -> bool {
    store::claim_revision_matches(paths, id, revision)
}

fn claim_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 领取准备阶段的错误是否需要中止本次领取：只有风控信号（3012 / unusual activity）
/// 必须停下并进入冷却；其他预览失败按 beta.11 约定降级（本地列表不决定最终资格）。
fn claim_prepare_abort(error: &str) -> Option<String> {
    risk::is_risk_signal(error).then(|| i18n::trf("err.risk.preview", &[("err", error)]))
}

/// Tauri 失败结果的唯一构造点：业务码、terminal、challenge、nextAt、unknown 与会话
/// 归属字段必须一起出现，自动队列/到点领取和界面都从这里取。
fn claim_failure_result(
    summary: &claim_session::ClaimSummary,
    error: &claim::ClaimError,
    cooling_secs: i64,
) -> Value {
    let mut payload = claim::failure_payload(
        &summary.account_id,
        &summary.account_name,
        &summary.plan_name,
        error,
    );
    if let Some(object) = payload.as_object_mut() {
        object.insert("claimId".into(), json!(summary.claim_id));
        object.insert("planId".into(), json!(summary.plan_id));
        object.insert("revision".into(), json!(summary.revision));
        object.insert("unknown".into(), json!(error.unknown));
        if cooling_secs > 0 {
            object.insert("coolingSecs".into(), json!(cooling_secs));
        }
    }
    payload
}

fn finish_claim(claim_id: &str, phase: claim_session::ClaimPhase, result: Option<Value>) -> bool {
    claim_registry_guard()
        .as_mut()
        .map(|registry| registry.finish(claim_id, phase, result))
        .unwrap_or(false)
}

fn claim_summary_json(claim_id: &str) -> Option<Value> {
    let registry = claim_registry_guard();
    let summary = registry.as_ref()?.status(claim_id)?;
    serde_json::to_value(summary).ok()
}

fn store_guard() -> std::sync::MutexGuard<'static, ()> {
    match STORE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 在阻塞线程池里跑必须同步等待的工作（ureq 网络请求、进程操作等）。
///
/// 纪律：`async` 命令里**禁止**直接执行阻塞 I/O。Tauri 的 async 命令跑在异步运行时的
/// worker 线程上，一次 `claim_refresh` 最长阻塞 2×10s（激活事件）+ 25s（preview）+
/// 25s（marketing）；并发点几个账号就会把所有 worker 占满，`get_state` / `set_behavior`
/// 等命令全部排队 → 界面整体卡死、额度区永远「查询中…」。
async fn run_blocking<T, F>(f: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| i18n::trf("err.internal", &[("e", &e.to_string())]))?
}

fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect()
}

fn tray_menu_inner(app: &AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    let paths = Paths::detect();
    let state = match store::get_state(&paths) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tray state error: {e}");
            return MenuBuilder::new(app)
                .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
                .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?)
                .build();
        }
    };

    let mut b = MenuBuilder::new(app);
    // 有新活动时，托盘菜单顶部给出醒目入口（tooltip 小字太弱，菜单更不易被忽略）
    if let Some(extra) = activity_tip_guard().clone().filter(|t| !t.trim().is_empty()) {
        let label = format!("【新活动】{extra}");
        b = b
            .item(&MenuItem::with_id(app, "activity", &label, true, None::<&str>)?)
            .separator();
    }
    let b = b
        .item(&MenuItem::with_id(app, "show", &i18n::tr("tray.show"), true, None::<&str>)?)
        .item(&MenuItem::with_id(app, "capture", &i18n::tr("tray.capture"), state.live_logged_in, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "launch", &i18n::tr("tray.launch"), state.zcode_path_ok && !state.zcode_running, None::<&str>)?)
        .item(&MenuItem::with_id(app, "kill", &i18n::tr("tray.kill"), state.zcode_running, None::<&str>)?)
        .separator()
        .item(&MenuItem::with_id(app, "quit", &i18n::tr("tray.quit"), true, None::<&str>)?);
    b.build()
}

pub fn rebuild_tray(app: &AppHandle) {
    let app2 = app.clone();
    let res = app.run_on_main_thread(move || {
        if let Some(tray) = app2.tray_by_id(TRAY_ID) {
            let paths = Paths::detect();
            let tip = match store::get_state(&paths) {
                Ok(s) => {
                    let cur = s
                        .accounts
                        .iter()
                        .find(|a| a.is_active)
                        .map(|a| a.name.clone())
                        .or_else(|| s.live_identity.as_ref().and_then(|i| i.label()))
                        .unwrap_or_else(|| if s.live_logged_in { i18n::tr("tray.unsaved") } else { i18n::tr("tray.logged_out") });
                    format!("Z·SWITCH · {cur}")
                }
                Err(_) => "Z·SWITCH".into(),
            };
            let extra = activity_tip_guard().clone().filter(|t| !t.trim().is_empty());
            let has_extra = extra.is_some();
            let tip = match &extra {
                // 把「新活动」放最前面：tooltip 前面部分更容易被看见
                Some(e) => format!("【新活动】{e} · {tip}"),
                None => tip,
            };
            let _ = tray.set_tooltip(Some(&tip));
            // Windows：仅 NIM_MODIFY 更新 szTip 有时不刷新，重设一次当前图标可强制 shell 读新提示
            if has_extra {
                if let Some(ic) = app2.default_window_icon().cloned() {
                    let _ = tray.set_icon(Some(ic));
                }
            }
            if let Ok(menu) = tray_menu_inner(&app2) {
                let _ = tray.set_menu(Some(menu));
            }
        }
    });
    if let Err(e) = res {
        eprintln!("rebuild_tray: {e}");
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn run_tray_action(app: AppHandle, action: String) {
    std::thread::spawn(move || {
        let paths = Paths::detect();
        let mut emit_payload = json!({ "action": action });
        let _guard = store_guard();
        let result: Result<serde_json::Value, String> = match action.as_str() {
            "capture" => store::capture_current(&paths, None).map(|a| json!({ "name": a.name })),
            "launch" => {
                let (p, ok) = effective_zcode_path(&paths);
                if ok { store::launch_zcode(&p).map(|_| json!({})) } else { Err(i18n::trf("err.zcode.path_invalid", &[("p", &p)])) }
            }
            "kill" => match store::kill_zcode() {
                Ok(true) => Ok(json!({})),
                Ok(false) => Err(i18n::tr("err.zcode.kill_timeout")),
                Err(e) => Err(e),
            },
            _ => Ok(json!({})),
        };
        match result {
            Ok(v) => {
                emit_payload["ok"] = json!(true);
                emit_payload["result"] = v;
            }
            Err(e) => {
                emit_payload["ok"] = json!(false);
                emit_payload["error"] = json!(e);
            }
        }
        let _ = app.emit("tray-action", &emit_payload);
        rebuild_tray(&app);
    });
}

#[tauri::command]
async fn get_state() -> Result<AppState, String> {
    store::get_state(&Paths::detect())
}

#[tauri::command]
fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

#[tauri::command]
async fn capture_current(app: AppHandle, name: Option<String>) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::capture_current(&Paths::detect(), name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn rename_account(app: AppHandle, id: String, name: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::rename_account(&Paths::detect(), &id, &name);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn delete_account(app: AppHandle, id: String) -> Result<(), String> {
    let _guard = store_guard();
    let r = store::delete_account(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn update_account_from_live(app: AppHandle, id: String) -> Result<Account, String> {
    let _guard = store_guard();
    let r = store::update_account_from_live(&Paths::detect(), &id);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn switch_to(app: AppHandle, id: String, force: bool, restart: bool) -> Result<SwitchResult, String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let hot = load_settings(&paths).hot_switch();
    let r = store::switch_to(&paths, &id, force, restart, hot);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn get_live_quota() -> Result<quota::QuotaOverview, String> {
    run_blocking(|| store::live_quota(&Paths::detect())).await
}

/// 营销活动按设备标识去重：投递过的 id 不再看到同一档活动。换新 id 等效于
/// 手动删除 ZCode 的 telemetry-state.json，但不动客户端自身的设备身份。
#[tauri::command]
async fn reset_device(id: String) -> Result<serde_json::Value, String> {
    let account_id = id.clone();
    let mid = run_blocking(move || {
        let paths = Paths::detect();
        store::reset_virtual_device_mid(&paths, &account_id)
    })
    .await?;
    Ok(json!({ "id": id, "deviceMid": mid }))
}

#[tauri::command]
async fn get_account_quota(id: String, force: Option<bool>) -> Result<quota::QuotaOverview, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        if risk::is_cooling(&paths, &id) && !force.unwrap_or(false) {
            let mins = (risk::cooling_remaining_secs(&paths, &id) + 59) / 60;
            return Err(i18n::trf("err.risk.cooling", &[("mins", &mins.to_string())]));
        }
        let r = store::account_quota(&paths, &id);
        if let Err(e) = &r {
            risk::observe_upstream_failure(&paths, &id, e);
        }
        r
    })
    .await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimPreviewResult {
    account_id: String,
    revision: String,
    plans: Vec<claim::ClaimPlan>,
    /// live 行专用：本结果依据的后端登录快照（与 get_state 的 live_hash 同源）。
    /// 前端据此拒绝「界面已不是这个登录」的迟到结果。
    #[serde(rename = "liveHash", skip_serializing_if = "Option::is_none")]
    live_hash: Option<String>,
}

#[tauri::command]
async fn claim_preview(id: String, revision: Option<String>) -> Result<ClaimPreviewResult, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        if risk::is_cooling(&paths, &id) {
            let mins = (risk::cooling_remaining_secs(&paths, &id) + 59) / 60;
            return Err(i18n::trf("err.risk.cooling", &[("mins", &mins.to_string())]));
        }
        let snapshot = {
            let _guard = store_guard();
            claim_snapshot(&paths, &id, revision.as_deref())?
        };
        if snapshot.device_initialized && revision.is_some() {
            // The device id changed the account revision before any HTTP request.
            return Err(claim::scope_payload(&id, &snapshot.revision, true));
        }
        let plans = claim::collect_claimable_plans(
            &paths.home,
            &snapshot.account.credentials,
            snapshot.account.config.as_ref(),
            snapshot.account.virtual_device_mid.clone().unwrap_or_default(),
        )
        .map_err(|e| {
            risk::observe_upstream_failure(&paths, &id, &e);
            e
        })?;
        let unchanged = {
            let _guard = store_guard();
            claim_revision_ok(&paths, &id, &snapshot.revision)
        };
        if !unchanged {
            return Err(claim::scope_payload(&id, &snapshot.revision, true));
        }
        Ok(ClaimPreviewResult {
            account_id: id.clone(),
            revision: snapshot.revision,
            plans,
            live_hash: (id == store::LIVE_ID)
                .then(|| store::live_snapshot_hash(&paths))
                .flatten(),
        })
    })
    .await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimCurrentResult {
    plans: Vec<claim::ActivePlan>,
    history: Vec<Value>,
    /// 无身份的旧历史：单独返回，由用户显式确认或清理，不并入当前登录。
    #[serde(rename = "legacyHistory")]
    legacy_history: Vec<Value>,
    /// 本结果依据的后端快照代次与身份：前端据此判断「这条结果是不是当前界面上的账号」。
    revision: String,
    #[serde(rename = "liveHash", skip_serializing_if = "Option::is_none")]
    live_hash: Option<String>,
    #[serde(rename = "identityScope", skip_serializing_if = "Option::is_none")]
    identity_scope: Option<String>,
}

#[tauri::command]
async fn claim_current(id: String) -> Result<ClaimCurrentResult, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        let secret = zcrypto::default_secret(&paths.home);
        // 领取历史按「账号 + 真实登录身份」隔离：同一个账号槽保存成另一登录后，
        // 不得继续看到上一个登录的领取记录（live 行同理）。
        let identity_for = |account: &store::ClaimSnapshot| -> Option<String> {
            Some(claim_session::identity_scope(&account.account.credentials, &secret))
        };
        let live_hash_for = |snapshot: &store::ClaimSnapshot| -> Option<String> {
            // 必须由同一份快照派生：再次读 live 文件会与 revision/identity 来自不同时刻。
            (snapshot.account.id == store::LIVE_ID)
                .then(|| store::canonical_hash(&snapshot.account.credentials))
        };
        // 冷却期零上游流量：current 只回本地历史，不发上游请求（避免冷却被反复刷新击穿）
        if risk::is_cooling(&paths, &id) {
            let snapshot = {
                let _guard = store_guard();
                claim_snapshot(&paths, &id, None).ok()
            };
            let identity = snapshot.as_ref().and_then(identity_for);
            return Ok(ClaimCurrentResult {
                plans: Vec::new(),
                history: store::claim_history_for_identity(&paths, &id, identity.as_deref()),
                legacy_history: store::claim_history_legacy_for(&paths, &id),
                revision: snapshot.as_ref().map(|s| s.revision.clone()).unwrap_or_default(),
                live_hash: snapshot.as_ref().and_then(live_hash_for),
                identity_scope: identity,
            });
        }
        // Also covers the live row: claim_snapshot borrows live credentials + telemetry
        // device id and never reads accounts/live.json.
        let snapshot = {
            let _guard = store_guard();
            claim_snapshot(&paths, &id, None)?
        };
        let identity = identity_for(&snapshot);
        let plans = claim::fetch_active_plans(
            &paths.home,
            &snapshot.account.credentials,
            snapshot.account.config.as_ref(),
            snapshot.account.virtual_device_mid.clone(),
        )
        .map_err(|e| {
            risk::observe_upstream_failure(&paths, &id, &e);
            e
        })?;
        let history = store::claim_history_for_identity(&paths, &id, identity.as_deref());
        Ok(ClaimCurrentResult {
            plans,
            history,
            legacy_history: store::claim_history_legacy_for(&paths, &id),
            revision: snapshot.revision.clone(),
            live_hash: live_hash_for(&snapshot),
            identity_scope: identity,
        })
    })
    .await
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRefreshResult {
    account_id: String,
    revision: String,
    plans: Vec<claim::ClaimPlan>,
    activated: bool,
    activation_error: Option<String>,
    #[serde(rename = "liveHash", skip_serializing_if = "Option::is_none")]
    live_hash: Option<String>,
}

#[tauri::command]
async fn claim_refresh(id: String, revision: Option<String>, force: Option<bool>) -> Result<ClaimRefreshResult, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        if risk::is_cooling(&paths, &id) && !force.unwrap_or(false) {
            let mins = (risk::cooling_remaining_secs(&paths, &id) + 59) / 60;
            return Err(i18n::trf("err.risk.cooling", &[("mins", &mins.to_string())]));
        }
        let snapshot = {
            let _guard = store_guard();
            claim_snapshot(&paths, &id, revision.as_deref())?
        };
        if snapshot.device_initialized && revision.is_some() {
            return Err(claim::scope_payload(&id, &snapshot.revision, true));
        }
        let mid = snapshot.account.virtual_device_mid.clone().unwrap_or_default();
        let (activated, activation_error) =
            match claim::telemetry_user_id(&paths.home, &snapshot.account.credentials) {
                Some(uid) => match claim::report_activation_events(&uid, &mid) {
                    Ok(()) => (true, None),
                    Err(e) => {
                        // 激活上报的明确风控同样要进入冷却并中止本次领取；
                        // 普通网络故障保持既有降级（只记录 activation_error）。
                        risk::observe_upstream_failure(&paths, &id, &e);
                        if let Some(message) = claim_prepare_abort(&e) {
                            return Err(message);
                        }
                        (false, Some(e))
                    }
                },
                None => (false, None),
            };
        let plans = claim::collect_claimable_plans(
            &paths.home,
            &snapshot.account.credentials,
            snapshot.account.config.as_ref(),
            mid,
        )
        .map_err(|e| {
            risk::observe_upstream_failure(&paths, &id, &e);
            e
        })?;
        let unchanged = {
            let _guard = store_guard();
            claim_revision_ok(&paths, &id, &snapshot.revision)
        };
        if !unchanged {
            // The activation report was already sent, so this must not auto-retry.
            return Err(claim::scope_payload(&id, &snapshot.revision, false));
        }
        Ok(ClaimRefreshResult {
            account_id: id.clone(),
            revision: snapshot.revision,
            plans,
            activated,
            activation_error,
            live_hash: (id == store::LIVE_ID)
                .then(|| store::live_snapshot_hash(&paths))
                .flatten(),
        })
    })
    .await
}

#[tauri::command]
async fn claim_start(
    app: AppHandle,
    id: String,
    revision: Option<String>,
    plan_id: String,
    claim_id: Option<String>,
    mode: Option<String>,
) -> Result<serde_json::Value, String> {
    let claim_id = claim_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let mode = match mode.as_deref() {
        Some("auto") => "auto",
        Some("all") => "all",
        _ => "manual",
    };
    let is_auto = mode == "auto";
    let paths = Paths::detect();
    let snapshot = {
        let _guard = store_guard();
        claim_snapshot(&paths, &id, revision.as_deref())?
    };
    if snapshot.device_initialized && revision.is_some() {
        return Err(claim::scope_payload(&id, &snapshot.revision, true));
    }
    // 自动轮次受风控冷却约束；手动领取是用户显式意图，放行
    if is_auto && risk::is_cooling(&paths, &id) {
        return Err(i18n::tr("err.risk.cooling_auto"));
    }
    // 领取准备守卫：激活上报与验证码配置的明确风控都进入冷却并中止（不开窗、不提交）。
    // 必须在注册会话之前完成，失败时不留下任何可继续执行的会话。
    let activation_home = paths.home.clone();
    let activation_creds = snapshot.account.credentials.clone();
    let activation_mid = snapshot.account.virtual_device_mid.clone().unwrap_or_default();
    let guard_paths = Paths { home: paths.home.clone(), data_root: paths.data_root.clone() };
    let guard_id = id.clone();
    run_blocking(move || {
        claim_start_guards(
            &guard_paths,
            &guard_id,
            || match activation_report_pending(&activation_home, &activation_creds, &activation_mid) {
                Some(user_id) => claim::report_activation_events(&user_id, &activation_mid),
                None => Ok(()),
            },
            || claim::fetch_captcha_config_checked().map(|_| ()),
        )
    })
    .await?;
    let secret = zcrypto::default_secret(&paths.home);
    let scope = claim_session::identity_scope(&snapshot.account.credentials, &secret);
    let now = claim_now_ms();
    let mut session = claim_session::ClaimSession {
        claim_id: claim_id.clone(),
        account_id: id.clone(),
        account_name: snapshot.account.name.clone(),
        revision: snapshot.revision.clone(),
        identity_scope: scope.clone(),
        plan_id: plan_id.clone(),
        plan_name: String::new(),
        auto: is_auto,
        created_at: now,
        updated_at: now,
        phase: claim_session::ClaimPhase::Preparing,
        replaces: None,
        material: claim_session::ClaimMaterial {
            credentials: snapshot.account.credentials.clone(),
            config: snapshot.account.config.clone(),
            device_mid: snapshot.account.virtual_device_mid.clone().unwrap_or_default(),
        },
    };
    {
        let mut registry = claim_registry_guard();
        let Some(registry) = registry.as_mut() else {
            return Err(i18n::tr("err.claim.none_pending"));
        };
        if registry.blocks(&scope, &plan_id) {
            // An unresolved submission only permits one acknowledged manual retry.
            match registry.gate(mode, &scope, &plan_id) {
                claim_session::StartGate::Allowed(replaced) => session.replaces = replaced,
                claim_session::StartGate::Unconfirmed => return Err(i18n::tr("err.claim.unconfirmed")),
            }
        }
        match registry.try_start(session) {
            Ok(_) => {}
            Err(claim_session::StartError::Busy) => return Err(i18n::tr("err.claim.busy")),
            Err(claim_session::StartError::Recovery) => {
                return Err(i18n::tr("err.claim.recovery_blocked"))
            }
        }
    }

    let home = paths.home.clone();
    let credentials = snapshot.account.credentials.clone();
    let config = snapshot.account.config.clone();
    let mid = snapshot.account.virtual_device_mid.clone();
    let wanted = plan_id.clone();
    let prepared = run_blocking(move || {
        claim::collect_claimable_plans(
            &home,
            &credentials,
            config.as_ref(),
            mid.unwrap_or_default(),
        )
        .map_err(|e| {
            risk::observe_upstream_failure(&paths, &id, &e);
            e
        })
    })
    .await;
    // 风控命中（3012 / unusual activity）：记录冷却并中止本次准备——不开验证码窗口、
    // 不发 claim POST。普通预览失败仍按 beta.11 的约定降级（本地列表不决定最终资格）。
    if let Err(error) = &prepared {
        if let Some(message) = claim_prepare_abort(error) {
            finish_claim(&claim_id, claim_session::ClaimPhase::Failed, None);
            return Err(message);
        }
    }
    // A campaign read only supplies the display name: the server decides whether this
    // identity may claim the plan right now, so an empty or failed read must not block
    // a claim the user already clicked. The session stays owned by this claim either way.
    let display = claim::claim_display_name(prepared, &wanted);
    let verifying = {
        let mut registry = claim_registry_guard();
        match registry.as_mut() {
            Some(registry) => {
                registry.update_plan_name(&claim_id, &display);
                registry.mark_verifying(&claim_id)
            }
            None => false,
        }
    };
    if !verifying {
        // The session was cancelled while preparing: never open a window for it.
        return Err(i18n::tr("err.claim.cancelled"));
    }
    if let Err(error) = open_captcha_window(&app, &claim_id, is_auto) {
        finish_claim(&claim_id, claim_session::ClaimPhase::Failed, None);
        return Err(error);
    }
    claim_summary_json(&claim_id).ok_or_else(|| i18n::tr("err.claim.none_pending"))
}

/// 返回本次需要上报的 user_id（每个 登录+设备 只上报一次）。
fn activation_report_pending(
    home: &std::path::Path,
    credentials: &Value,
    device_mid: &str,
) -> Option<String> {
    static REPORTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let user_id = claim::telemetry_user_id(home, credentials)?;
    let key = format!("{user_id}:{device_mid}");
    {
        let mut done = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
        if done.iter().any(|k| k == &key) {
            return None;
        }
        done.push(key);
    }
    Some(user_id)
}

/// 归属前校验：用户确认时固定的身份作用域（live 另加快照哈希）必须与当前快照一致。
/// 不一致时安全中止，旧记录保持未归属——绝不归给另一个身份。
fn legacy_adopt_identity(
    snapshot: &store::ClaimSnapshot,
    requested_scope: Option<&str>,
    requested_live_hash: Option<&str>,
    secret: &str,
) -> Result<String, String> {
    if snapshot.account.id == store::LIVE_ID {
        let current = store::canonical_hash(&snapshot.account.credentials);
        if requested_live_hash != Some(current.as_str()) {
            return Err(claim::scope_payload(&snapshot.account.id, &snapshot.revision, true));
        }
    }
    let identity = claim_session::identity_scope(&snapshot.account.credentials, secret);
    if requested_scope != Some(identity.as_str()) {
        return Err(claim::scope_payload(&snapshot.account.id, &snapshot.revision, true));
    }
    Ok(identity)
}

/// claim_start 的准备守卫：激活上报与验证码配置。两者的明确风控都要进入同一冷却状态机
/// 并中止本次领取；普通网络故障保持既有降级（激活忽略、配置回退）。
/// 生产命令与测试使用同一实现，副作用通过闭包注入。
fn claim_start_guards<A, C>(paths: &Paths, id: &str, activation: A, captcha: C) -> Result<(), String>
where
    A: FnOnce() -> Result<(), String>,
    C: FnOnce() -> Result<(), String>,
{
    if let Err(error) = activation() {
        risk::observe_upstream_failure(paths, id, &error);
        if let Some(message) = claim_prepare_abort(&error) {
            return Err(message);
        }
    }
    if let Err(error) = captcha() {
        risk::observe_upstream_failure(paths, id, &error);
        if let Some(message) = claim_prepare_abort(&error) {
            return Err(message);
        }
    }
    Ok(())
}

#[tauri::command]
async fn claim_captcha_config() -> Result<claim::CaptchaConfig, String> {
    run_blocking(claim::fetch_captcha_config).await
}

#[tauri::command]
async fn claim_captcha_submit(
    app: AppHandle,
    claim_id: String,
    param: String,
    region: Option<String>,
) -> Result<serde_json::Value, String> {
    let summary = {
        let registry = claim_registry_guard();
        registry.as_ref().and_then(|r| r.status(&claim_id))
    }
    .ok_or_else(|| i18n::tr("err.claim.none_pending"))?;
    let material = {
        let mut registry = claim_registry_guard();
        let Some(registry) = registry.as_mut() else {
            return Err(i18n::tr("err.claim.none_pending"));
        };
        match registry.begin_submit(&claim_id) {
            Ok(material) => material,
            Err(claim_session::SubmitError::PersistFailed) => {
                registry.finish(&claim_id, claim_session::ClaimPhase::Failed, None);
                return Err(i18n::tr("err.claim.marker_failed"));
            }
            Err(claim_session::SubmitError::NotReady) => {
                return Err(i18n::tr("err.claim.already_submitted"))
            }
            Err(_) => return Err(i18n::tr("err.claim.none_pending")),
        }
    };

    let paths = Paths::detect();
    let home = paths.home.clone();
    let plan_id = summary.plan_id.clone();
    let submitted = match tauri::async_runtime::spawn_blocking(move || {
        claim::submit_claim(
            &home,
            &material.credentials,
            material.config.as_ref(),
            &plan_id,
            &param,
            region.as_deref(),
            Some(material.device_mid),
        )
    })
    .await
    {
        Ok(result) => result,
        // The worker died before a definitive answer: the POST may still have landed.
        Err(e) => Err(claim::ClaimError {
            code: -1,
            message: i18n::trf("err.internal", &[("e", &e.to_string())]),
            next_at: None,
            unknown: true,
        }),
    };

    // 3007（验证码被拒/参数过期）：**不再换码自动重试**。
    // 参考实测结论（zcode2api 2.6.5 整改）：拒码本身就是上游风险信号，换码连打只会加剧，
    // 且会白耗一次唯一重试额度。这里直接走终态失败，让用户重新发起领取（重新取验证码）。
    if let Err(e) = &submitted {
        if e.is_captcha_rejected() {
            flowlog::log(
                "claim",
                "captcha-rejected-3007",
                &format!("code={} 不自动重试", e.code),
            );
        }
    }

    let payload = match submitted {
        Ok(value) => {
            let ms = |k: &str| -> Option<i64> {
                value
                    .pointer(&format!("/data/plan/{k}"))
                    .and_then(quota::timestamp_millis)
            };
            let server_time = value
                .pointer("/data/server_time")
                .and_then(quota::timestamp_millis);
            let entitlement_starts = value
                .pointer("/data/plan")
                .map(quota::entitlement_start_times)
                .unwrap_or_default();
            let mut payload = json!({
                "ok": true,
                "claimId": claim_id,
                "accountId": summary.account_id,
                "accountName": summary.account_name,
                "revision": summary.revision,
                "planId": summary.plan_id,
                "planName": summary.plan_name,
                "startsAt": ms("starts_at"),
                "entitlementStarts": entitlement_starts,
                "endsAt": ms("ends_at"),
                "serverTime": server_time,
            });
            // 权益真正生效的时刻在 entitlement 上（0 = 立即生效），starts_at 只是活动开始
            if let Some(effective_at) = entitlement_starts
                .iter()
                .filter_map(|s| s.effective_at)
                .filter(|ms| *ms > 0)
                .min()
                .or_else(|| {
                    entitlement_starts
                        .iter()
                        .filter_map(|s| s.effective_at)
                        .min()
                })
            {
                payload["effectiveAt"] = json!(effective_at);
            }
            // 领取成功落历史（best-effort），供「已领取生效中」展示
            store::record_claim_history(
                &paths,
                &summary.account_id,
                &summary.identity_scope,
                &summary.plan_id,
                &summary.plan_name,
            );
            finish_claim(&claim_id, claim_session::ClaimPhase::Succeeded, Some(payload.clone()));
            payload
        }
        Err(error) => {
            // 提交阶段的风控信号（3012 / unusual activity）也必须进入冷却状态机：
            // 否则下一轮自动/到点领取会立刻再打一次同一个风控出口。
            risk::observe_upstream_failure(&paths, &summary.account_id, &error.message);
            // 所有失败结果统一构造：业务码、terminal、challenge、nextAt 与 unknown
            // 都必须原样到达界面与会话持久化，不能只在 unknown 分支保留。
            let payload = claim_failure_result(
                &summary,
                &error,
                risk::cooling_remaining_secs(&paths, &summary.account_id),
            );
            let phase = if error.unknown {
                claim_session::ClaimPhase::Unknown
            } else {
                claim_session::ClaimPhase::Failed
            };
            finish_claim(&claim_id, phase, Some(payload.clone()));
            payload
        }
    };
    close_captcha_window(&app, &claim_id);
    let _ = app.emit("claim://result", &payload);
    Ok(payload)
}

/// 只读本地：批量查询各账号「无身份旧历史」条数（启动时用于保守压制自动领取）。
/// 读取/解析失败必须返回错误：前端把「未知」当成需要阻止，而不是「没有旧记录」。
#[tauri::command]
async fn claim_legacy_count(ids: Vec<String>) -> Result<Vec<Value>, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        let _guard = store_guard();
        let mut rows = Vec::with_capacity(ids.len());
        for id in ids {
            let count = store::claim_history_legacy_check(&paths, &id)?.len();
            rows.push(json!({ "accountId": id, "count": count }));
        }
        Ok(rows)
    })
    .await
}

/// 用户显式把「无身份旧历史」归属到当前登录。前端固定确认时的账号代次与身份作用域，
/// 后端在写入前验证仍是同一快照：账号在操作中被切换时中止，绝不归给另一个身份。
#[tauri::command]
async fn claim_adopt_legacy_history(
    id: String,
    revision: Option<String>,
    identity_scope: Option<String>,
    live_hash: Option<String>,
) -> Result<store::LegacyAdoptOutcome, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        let secret = zcrypto::default_secret(&paths.home);
        let snapshot = {
            let _guard = store_guard();
            claim_snapshot(&paths, &id, revision.as_deref())?
        };
        let identity = legacy_adopt_identity(
            &snapshot,
            identity_scope.as_deref(),
            live_hash.as_deref(),
            &secret,
        )?;
        let _guard = store_guard();
        store::adopt_claim_history_legacy(&paths, &id, &identity)
    })
    .await
}

/// 用户显式清理某账号的「无身份旧历史」：只删除无法归属的条目，当前身份的历史保留。
/// 读取、解析与写入失败都传播给界面（不得提示成功或放开自动领取）。
#[tauri::command]
async fn claim_drop_legacy_history(id: String) -> Result<usize, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        let _guard = store_guard();
        store::drop_claim_history_legacy(&paths, &id)
    })
    .await
}

#[tauri::command]
async fn claim_cancel(app: AppHandle, claim_id: String) -> Result<serde_json::Value, String> {
    let outcome = {
        let mut registry = claim_registry_guard();
        match registry.as_mut() {
            Some(registry) => registry.cancel(&claim_id),
            None => claim_session::CancelOutcome {
                phase: claim_session::ClaimPhase::Cancelled.as_str().to_string(),
                cancelled: false,
                submission_in_flight: false,
            },
        }
    };
    if outcome.cancelled {
        close_captcha_window(&app, &claim_id);
        let _ = app.emit("claim://cancelled", json!({ "claimId": claim_id }));
    }
    Ok(serde_json::to_value(outcome).unwrap_or(Value::Null))
}

#[tauri::command]
async fn claim_status(claim_id: String) -> Result<serde_json::Value, String> {
    Ok(claim_summary_json(&claim_id).unwrap_or(Value::Null))
}

#[tauri::command]
async fn claim_sessions() -> Result<serde_json::Value, String> {
    let registry = claim_registry_guard();
    let Some(registry) = registry.as_ref() else {
        return Ok(json!({ "sessions": [], "recovery": { "ok": true } }));
    };
    Ok(json!({
        "sessions": registry.sessions(),
        "recovery": {
            "ok": registry.recovery_error().is_none(),
            "error": registry.recovery_error(),
        }
    }))
}

#[tauri::command]
async fn claim_acknowledge_unknown(claim_id: String) -> Result<bool, String> {
    let mut registry = claim_registry_guard();
    match registry.as_mut() {
        Some(registry) => Ok(registry.allow_manual_retry(&claim_id)),
        None => Ok(false),
    }
}

/// 当前待领取的那一笔（用于「只取消自己发起的领取」）。向后兼容旧前端入口。
#[tauri::command]
fn claim_pending() -> Option<serde_json::Value> {
    let registry = claim_registry_guard();
    registry
        .as_ref()?
        .sessions()
        .into_iter()
        .find(|s| ["preparing", "verifying", "submitting"].contains(&s.status.as_str()))
        .map(|s| json!({ "accountId": s.account_id, "planId": s.plan_id }))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityProbeItem {
    account_id: String,
    name: String,
    plans: Vec<claim::ClaimPlan>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ActivityProbeResult {
    items: Vec<ActivityProbeItem>,
    /// 因风控冷却 / 账号读取失败被跳过的账号（前端用来显示「已跳过」而不是「无活动」）
    skipped: Vec<String>,
}

/// 只读活动探测：只请求 marketing/touch，不领取、不写 claim-history、不发 activation 事件。
/// 冷却中的账号直接跳过（零上游流量）；账号选择与节奏由前端调度器负责。
#[tauri::command]
async fn activity_probe(ids: Vec<String>) -> Result<ActivityProbeResult, String> {
    run_blocking(move || {
        let paths = Paths::detect();
        let mut items = Vec::new();
        let mut skipped = Vec::new();
        for id in ids {
            if risk::is_cooling(&paths, &id) {
                skipped.push(id);
                continue;
            }
            let Ok(mid) = store::ensure_virtual_device_mid(&paths, &id) else {
                skipped.push(id);
                continue;
            };
            let Ok(acc) = load_account(&paths, &id) else {
                skipped.push(id);
                continue;
            };
            let plans = match claim::probe_marketing_checked(
                &paths.home,
                &acc.credentials,
                acc.config.as_ref(),
                Some(mid),
            ) {
                Ok(plans) => plans,
                Err(error) => {
                    // 活动探测同样会撞风控：记录冷却，避免下一轮继续打同一出口。
                    risk::observe_upstream_failure(&paths, &id, &error);
                    let _ = append_rolling_log(
                        "probe.log",
                        2000,
                        &format!("probe risk id={id}: {error}"),
                    );
                    Vec::new()
                }
            };
            items.push(ActivityProbeItem { account_id: id, name: acc.name, plans });
        }
        Ok(ActivityProbeResult { items, skipped })
    })
    .await
}

fn append_rolling_log(name: &str, cap: usize, message: &str) -> Result<(), String> {
    let dir = Paths::detect().store_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(name);
    let mut lines: Vec<String> = std::fs::read_to_string(&file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect();
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    lines.push(format!("[{ts}] {}", message.replace('\n', " ⏎ ")));
    if lines.len() > cap {
        let excess = lines.len() - cap;
        lines.drain(0..excess);
    }
    std::fs::write(&file, lines.join("\n") + "\n").map_err(|e| e.to_string())
}

/// 设置「新活动」提示：托盘 tooltip 附加文案（None 清除）。
/// 便携版拿不到 Windows Toast 时，悬停托盘图标是可见性兜底。
#[tauri::command]
async fn set_activity_tip(app: AppHandle, tip: Option<String>) -> Result<(), String> {
    let cleaned = tip.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    {
        let mut g = activity_tip_guard();
        *g = cleaned.clone();
    }
    rebuild_tray(&app);
    let _ = append_rolling_log(
        "probe.log",
        500,
        &format!("tray tip -> {:?}", cleaned),
    );
    if app.tray_by_id(TRAY_ID).is_none() {
        return Err("tray not found".into());
    }
    Ok(())
}

/// 原图标 + 右下角红点徽标（钉钉式）：红点出现/消失交替，比整图标染色自然。
fn badged_icon(base: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let (w, h) = (base.width(), base.height());
    let mut rgba = base.rgba().to_vec();
    let cx = w as f32 * 0.74;
    let cy = h as f32 * 0.74;
    let r = (h as f32 * 0.22).max(3.0);
    for y in 0..h {
        for x in 0..w {
            let dx = x as f32 - cx;
            let dy = y as f32 - cy;
            let d = (dx * dx + dy * dy).sqrt();
            if d <= r {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 235;
                rgba[i + 1] = 64;
                rgba[i + 2] = 56;
                rgba[i + 3] = 255;
            } else if d <= r + 1.5 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 255;
                rgba[i + 1] = 255;
                rgba[i + 2] = 255;
                rgba[i + 3] = 255;
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w, h)
}

/// 托盘图标交替闪烁（Windows 托盘没有闪烁 API，用图标交替模拟）。
/// 提醒态 = 原图标 + 右下角红点徽标，与常态来回切换 `times` 轮后恢复（0 = 直到点开活动）。
#[tauri::command]
async fn tray_flash(app: AppHandle, times: Option<u32>) -> Result<(), String> {
    use tauri::image::Image;
    let base = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| "no window icon".to_string())?;
    let (w, h) = (base.width(), base.height());
    let base: Image<'static> = Image::new_owned(base.rgba().to_vec(), w, h);
    let attention: Image<'static> = badged_icon(&base);
    // times: None/0 = 持续闪烁直到用户点开活动（tray_flash_stop），
    // Some(n) = 闪 n 轮后自动停止（仅测试用）。
    let rounds = times.unwrap_or(0);
    TRAY_FLASH_ACTIVE.store(true, Ordering::SeqCst);
    std::thread::spawn(move || {
        let mut count = 0u32;
        while TRAY_FLASH_ACTIVE.load(Ordering::SeqCst) {
            count += 1;
            if rounds > 0 && count > rounds * 2 {
                TRAY_FLASH_ACTIVE.store(false, Ordering::SeqCst);
                break;
            }
            let holder = app.clone();
            let inner = app.clone();
            let ic = attention.clone();
            let _ = holder.run_on_main_thread(move || {
                if let Some(t) = inner.tray_by_id(TRAY_ID) {
                    let _ = t.set_icon(Some(ic));
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(420));
            let holder = app.clone();
            let inner = app.clone();
            let ic = base.clone();
            let _ = holder.run_on_main_thread(move || {
                if let Some(t) = inner.tray_by_id(TRAY_ID) {
                    let _ = t.set_icon(Some(ic));
                }
            });
            std::thread::sleep(std::time::Duration::from_millis(420));
        }
        let _ = append_rolling_log("probe.log", 500, "tray flash done");
    });
    Ok(())
}

/// 停止托盘闪烁（用户已点开活动详情），并恢复常态图标。
#[tauri::command]
async fn tray_flash_stop(app: AppHandle) -> Result<(), String> {
    TRAY_FLASH_ACTIVE.store(false, Ordering::SeqCst);
    let owned = app.default_window_icon().map(|ic| {
        let (w, h) = (ic.width(), ic.height());
        tauri::image::Image::new_owned(ic.rgba().to_vec(), w, h)
    });
    if let Some(ic) = owned {
        let app2 = app.clone();
        let _ = app.run_on_main_thread(move || {
            if let Some(t) = app2.tray_by_id(TRAY_ID) {
                let _ = t.set_icon(Some(ic));
            }
        });
    }
    Ok(())
}

/// 探测留痕：`{home}/.zcode-switch/probe.log`（500 行滚动）。
/// 用于回答「开关开了但没探到」这类问题：能查到每轮选了谁、返回几条、是否被跳过。
#[tauri::command]
async fn log_probe(message: String) -> Result<(), String> {
    run_blocking(move || append_rolling_log("probe.log", 500, &message)).await
}

/// 前端异常落盘：`{home}/.zcode-switch/client-error.log`，最多保留 200 行。
/// 目的是让「界面冻结/报错」这类只能靠 DevTools 看到的问题可回传、可定位。
#[tauri::command]
async fn log_client_error(message: String) -> Result<(), String> {
    run_blocking(move || {
        let dir = Paths::detect().store_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let file = dir.join("client-error.log");
        let mut lines: Vec<String> = std::fs::read_to_string(&file)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        lines.push(format!("[{ts}] {}", message.replace('\n', " ⏎ ")));
        if lines.len() > 200 {
            let excess = lines.len() - 200;
            lines.drain(0..excess);
        }
        std::fs::write(&file, lines.join("\n") + "\n").map_err(|e| e.to_string())
    })
    .await
}

fn close_captcha_window(app: &AppHandle, claim_id: &str) {
    let label = format!("captcha-{claim_id}");
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.close();
    }
}

struct PendingOAuth {
    provider: String,
    state: String,
    flow: String,
}

#[derive(Clone)]
struct PollCfg {
    url: String,
    token: String,
    expires_at_ms: u128,
    interval_ms: u64,
}

static PENDING_OAUTH: Mutex<Option<PendingOAuth>> = Mutex::new(None);

fn pending_oauth_guard() -> std::sync::MutexGuard<'static, Option<PendingOAuth>> {
    match PENDING_OAUTH.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[tauri::command]
async fn oauth_providers() -> Result<Vec<oauth::OAuthProvider>, String> {
    Ok(oauth::OAUTH_PROVIDERS.to_vec())
}

#[tauri::command]
async fn oauth_begin(app: AppHandle, provider: String) -> Result<serde_json::Value, String> {
    if !oauth::OAUTH_PROVIDERS.iter().any(|p| p.id == provider) {
        return Err(i18n::trf("err.oauth.unknown_provider", &[("provider", &provider)]));
    }
    let proxy_url: Option<tauri::Url> = match load_settings(&Paths::detect()).auth_proxy() {
        Some(p) => {
            let norm = oauth::parse_proxy_url(p)?;
            Some(norm.parse::<tauri::Url>().map_err(|e| i18n::trf("err.proxy.invalid", &[("e", &e.to_string())]))?)
        }
        None => None,
    };
    *pending_oauth_guard() = None;
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.close();
    }
    let flow = uuid::Uuid::new_v4().to_string();
    flowlog::log(&flow, "begin", &format!("provider={provider} proxy={}", if proxy_url.is_some() { "on" } else { "off" }));
    let mid = uuid::Uuid::new_v4().to_string();
    let (p_init, m_init) = (provider.clone(), mid.clone());
    let init = match tauri::async_runtime::spawn_blocking(move || oauth::init_flow(&p_init, &m_init))
        .await
    {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            flowlog::log(&flow, "init-fail", &e);
            return Err(e);
        }
        Err(e) => {
            let m = i18n::trf("err.oauth.flow", &[("e", &e.to_string())]);
            flowlog::log(&flow, "init-fail", &m);
            return Err(m);
        }
    };
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let srv_flow = init.poll_url.rsplit('/').next().unwrap_or("");
        let expires_in = init.expires_at_ms.saturating_sub(now) / 1000;
        let authorize = init.authorize_url.split('?').next().unwrap_or("");
        let mode = if provider == "zai" { "official-login+relay" } else { "direct" };
        flowlog::log(
            &flow,
            "init-ok",
            &format!("mode={mode} authorize={authorize} server_flow={srv_flow} expires_in={expires_in}s interval={}ms", init.poll_interval_ms),
        );
    }
    // zai 第二段接力用规范化入口（chat.z.ai/auth?...）：直接打原始 /api/oauth/authorize
    // 会因缺少 chat.z.ai 会话被弹回登录页；走 /auth 可完成登录/SSO。
    // 非 zai 或未规范化时 authorize_url 就是服务端原始地址。
    let url = init.authorize_url.clone();
    // zai：默认直接打开本次 flow 的 CLI 授权入口（chat.z.ai/auth?client_id=<CLI>&...）。
    // 在该页用手机号 / 邮箱登录会直接建立 chat.z.ai 会话并续到授权/回调，不依赖官网跨站会话。
    let entry_url = url.clone();
    let poll_cfg = PollCfg {
        url: init.poll_url.clone(),
        token: init.poll_token.clone(),
        expires_at_ms: init.expires_at_ms,
        interval_ms: init.poll_interval_ms,
    };
    *pending_oauth_guard() = Some(PendingOAuth {
        provider: provider.clone(),
        state: init.state.clone(),
        flow: flow.clone(),
    });

    let login_root = app
        .path()
        .app_local_data_dir()
        .map_err(|e| i18n::trf("err.oauth.appdata", &[("e", &e.to_string())]))?
        .join("login-webview");
    sweep_login_profiles(&login_root);
    let profile_dir = login_root.join(&flow);

    let app2 = app.clone();
    let (provider2, state2, flow2, mid2) = (provider.clone(), init.state.clone(), flow.clone(), mid.clone());
    let flow_close = flow.clone();
    let relay_to = url.clone();
    let relay_to2 = url.clone();
    let relay_armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(provider == "zai"));
    let relay_armed2 = relay_armed.clone();
    let relay_armed3 = relay_armed.clone();
    let flow_relay = flow.clone();
    let flow_relay2 = flow.clone();
    let flow_nav = flow.clone();
    let log_nav = provider == "zai";
    let mut builder = tauri::WebviewWindowBuilder::new(
        &app,
        "login",
        tauri::WebviewUrl::External(entry_url.parse::<tauri::Url>().map_err(|e| i18n::trf("err.oauth.bad_authorize_url", &[("e", &e.to_string())]))?),
    )
    .title(i18n::tr("title.login"))
    .theme(Some(tauri::Theme::Dark))
    .inner_size(480.0, 680.0)
    .min_inner_size(420.0, 560.0)
    .resizable(true)
    .user_agent(oauth::LOGIN_WINDOW_UA)
    .data_directory(profile_dir);
    if let Some(u) = proxy_url {
        builder = builder.proxy_url(u);
    }
    // z.ai：邮箱 / 手机号两个表单在同一个登录页上（手机号默认隐藏），页内切换优先，
    // 页面每次加载后注入按钮条，用户随时可切回邮箱登录 / 邮箱注册
    if provider == "zai" {
        let assist = oauth::zai_email_assist_script(&init.authorize_url);
        let assist_load = assist.clone();
        let flow_load = flow.clone();
        builder = builder.on_page_load(move |wv, payload| {
            if let tauri::webview::PageLoadEvent::Finished = payload.event() {
                flowlog::log(&flow_load, "assist", "inject on-page-load");
                let _ = wv.eval(&assist_load);
            }
        });
        // 首次导航可能早于 on_page_load 注册生效，窗口起来后再补几次注入（脚本幂等，重复执行无副作用）
        let app_assist = app.clone();
        let flow_assist = flow.clone();
        std::thread::spawn(move || {
            let mut logged = false;
            for _ in 0..10 {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let ours = pending_oauth_guard().as_ref().map(|p| p.flow == flow_assist).unwrap_or(false);
                if !ours {
                    return;
                }
                let Some(w) = app_assist.get_webview_window("login") else { return };
                if w.eval(&assist).is_ok() && !logged {
                    flowlog::log(&flow_assist, "assist", "inject boost");
                    logged = true;
                }
            }
        });
    }
    builder
    .on_navigation(move |url| {
        if url.scheme() == "zcode" {
            let full = url.to_string();
            let (app3, p3, s3, f3, m3) = (app2.clone(), provider2.clone(), state2.clone(), flow2.clone(), mid2.clone());
            tauri::async_runtime::spawn(async move {
                finish_oauth(&app3, p3, s3, f3, m3, &full).await;
            });
            return false;
        }
        if log_nav && url.host_str().map_or(false, |h| h.ends_with("z.ai")) {
            // 只标记是否带 error 参数，不落 query（可能含 code / state 等敏感值）
            let mark = if url.query().map_or(false, |q| q.contains("error")) { " (error)" } else { "" };
            flowlog::log(&flow_nav, "nav", &format!("{}{}{}", url.host_str().unwrap_or(""), url.path(), mark));
        }
        let nav_host = url.host_str().unwrap_or("");
        let nav_path = url.path();
        // 接力状态跟随登录态：授权链路中收枪（避免打断正在进行的授权），
        // 回到登录页（含第三方登录入口）重新武装，保证再次登录完成后仍能取码
        if nav_host == "z.ai" || nav_host == "chat.z.ai" {
            if is_authorize_page(nav_path) {
                if relay_armed2.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    flowlog::log(&flow_relay, "relay-disarm", &format!("{nav_host}{nav_path}"));
                }
            } else if is_login_page(nav_path) {
                if !relay_armed2.load(std::sync::atomic::Ordering::SeqCst) {
                    flowlog::log(&flow_relay, "relay-rearm", &format!("{nav_host}{nav_path}"));
                    relay_armed2.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
        // zai 第一段（登录页）完成 → 接力到本次 flow 的授权入口取码（此时已登录，不再要求手机号）
        if (nav_host == "z.ai" || nav_host == "chat.z.ai")
            && !is_login_page(nav_path)
            && !is_authorize_page(nav_path)
            && relay_armed2.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            flowlog::log(
                &flow_relay,
                "zai-relay",
                &format!("nav {}{} -> authorize", url.host_str().unwrap_or(""), url.path()),
            );
            let target = relay_to.clone();
            let app4 = app2.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                if let Some(w) = app4.get_webview_window("login") {
                    if let Ok(t) = target.parse::<tauri::Url>() {
                        let _ = w.navigate(t);
                    }
                }
            });
            log_after_relay(app2.clone(), flow_relay.clone(), 5000);
        }
        true
    })
    .build()
    .map_err(|e| {
        let mut pending = pending_oauth_guard();
        if pending.as_ref().map(|p| p.flow == flow).unwrap_or(false) {
            *pending = None;
        }
        let m = i18n::trf("err.oauth.window", &[("e", &e.to_string())]);
        flowlog::log(&flow, "window-fail", &m);
        m
    })?;
    if let Some(w) = app.get_webview_window("login") {
        w.on_window_event(move |e| {
            if let tauri::WindowEvent::CloseRequested { .. } = e {
                let mut pending = pending_oauth_guard();
                if pending.as_ref().map(|p| p.flow == flow_close).unwrap_or(false) {
                    flowlog::log(&flow_close, "cancelled", "");
                    *pending = None;
                }
            }
        });
    }
    // 官网登录成功后走的是 SPA 内部路由（history.replaceState），on_navigation 收不到事件，
    // 所以再加一路轮询窗口 URL 兜底：一旦离开登录页、落到 z.ai 站点即接力取码
    if provider == "zai" {
        let app_relay = app.clone();
        let armed = relay_armed3;
        let target = relay_to2;
        let fl = flow_relay2;
        let guard_flow = flow.clone();
        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            while std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                let ours = pending_oauth_guard().as_ref().map(|p| p.flow == guard_flow).unwrap_or(false);
                if !ours {
                    return;
                }
                let Some(w) = app_relay.get_webview_window("login") else { return };
                let Ok(u) = w.url() else { continue };
                let host = u.host_str().unwrap_or("");
                let path = u.path();
                if !(host == "z.ai" || host == "chat.z.ai") {
                    continue;
                }
                if is_authorize_page(path) {
                    armed.store(false, std::sync::atomic::Ordering::SeqCst);
                    continue;
                }
                if is_login_page(path) {
                    armed.store(true, std::sync::atomic::Ordering::SeqCst);
                    continue;
                }
                if !armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    continue; // 已被 on_navigation 那路接力
                }
                std::thread::sleep(std::time::Duration::from_millis(1200)); // 留点时间让官网把回调收尾
                flowlog::log(&fl, "zai-relay", &format!("poll {host}{} -> authorize", u.path()));
                if let Ok(t) = target.parse::<tauri::Url>() {
                    let _ = w.navigate(t);
                }
                log_after_relay(app_relay.clone(), fl.clone(), 5000);
                return;
            }
        });
    }
    spawn_poll_loop(app.clone(), provider.clone(), flow.clone(), mid, poll_cfg);
    Ok(json!({ "opened": true, "provider": provider }))
}

/// 接力后留痕：几秒后把窗口实际落点写进日志，便于判断授权是否成功
fn log_after_relay(app: AppHandle, flow: String, delay_ms: u64) {
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(delay_ms));
        if let Some(w) = app.get_webview_window("login") {
            if let Ok(u) = w.url() {
                flowlog::log(&flow, "zai-after", &format!("{}{}", u.host_str().unwrap_or(""), u.path()));
            }
        }
    });
}

/// 登录 / 注册相关路径（含第三方入口与登录回调）：说明登录链路还没收尾，
/// zai 接力不能在这些页面触发，否则会打断官网自己的回调跳转。
fn is_login_page(path: &str) -> bool {
    ["/auth", "/login", "/signin", "/sign-in", "/signup", "/sign-up", "/register", "/reset", "/forgot", "/oauth"]
        .iter()
        .any(|p| path.starts_with(p))
}

/// 已在授权链路中（授权入口 / 授权确认页）：让页面自行走完，不能再接力，
/// 否则会把正在进行的授权打断，被服务端弹回登录页。
fn is_authorize_page(path: &str) -> bool {
    path.ends_with("/oauth/authorize")
}

fn sweep_login_profiles(root: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 24 * 3600);
    for e in entries.flatten() {
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() && meta.modified().map(|m| m < cutoff).unwrap_or(false) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[tauri::command]
async fn set_auth_proxy(app: AppHandle, on: bool, url: Option<String>) -> Result<(), String> {
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let trimmed = url.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let normalized = match trimmed {
            Some(s) => Some(oauth::parse_proxy_url(s)?),
            None => None,
        };
        if on && normalized.is_none() {
            return Err(i18n::tr("err.proxy.need_url"));
        }
        let mut s = load_settings(&paths);
        s.auth_proxy_on = Some(on);
        s.auth_proxy_url = normalized;
        save_settings(&paths, &s)?;
        net::set_proxy(s.auth_proxy().map(str::to_string));
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

async fn finish_oauth(app: &AppHandle, provider: String, state: String, flow: String, mid: String, callback_url: &str) {
    let result = {
        let provider = provider.clone();
        let state = state.clone();
        let flow = flow.clone();
        let mid = mid.clone();
        let callback_url = callback_url.to_string();
        tauri::async_runtime::spawn_blocking(move || -> Result<serde_json::Value, String> {
            {
                let pending = pending_oauth_guard();
                match pending.as_ref() {
                    Some(p) if p.provider == provider && p.state == state && p.flow == flow => {}
                    Some(_) | None => {
                        flowlog::log(&flow, "deeplink-superseded", "");
                        return Err("__superseded__".into());
                    }
                }
            }
            let (code, cb_state) = match oauth::parse_callback(&callback_url) {
                Ok(oauth::CallbackKind::Code { code, state }) => (code, state),
                Ok(oauth::CallbackKind::Attribution) => {
                    flowlog::log(&flow, "deeplink-attribution", "");
                    return Err("__attribution__".into());
                }
                Err(e) => {
                    flowlog::log(&flow, "deeplink-parse-fail", &e);
                    return Err(e);
                }
            };
            if cb_state != state {
                flowlog::log(&flow, "deeplink-state-mismatch", "");
                return Err(i18n::tr("err.oauth.state"));
            }
            let exchanged = match oauth::exchange_token(&provider, &code, &state, &mid) {
                Ok(v) => v,
                Err(e) => {
                    flowlog::log(&flow, "exchange-fail", &e);
                    return Err(e);
                }
            };
            match persist_oauth_account(&Paths::detect(), &provider, &exchanged["raw"], &flow, &mid, false) {
                Ok(v) => {
                    flowlog::log(
                        &flow,
                        "persist-ok",
                        &format!("channel=deeplink duplicate={}", v.get("duplicate").is_some()),
                    );
                    Ok(v)
                }
                Err(e) => {
                    if e != "__superseded__" {
                        flowlog::log(&flow, "persist-fail", &format!("channel=deeplink {e}"));
                    }
                    Err(e)
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(i18n::trf("err.oauth.flow", &[("e", &e.to_string())])))
    };
    if let Err(e) = &result {
        if deeplink_err_soft(e) {
            let ours = pending_oauth_guard().as_ref().map(|p| p.flow == flow).unwrap_or(false);
            if ours {
                flowlog::log(&flow, "soft-fail", e);
                let _ = app.emit("oauth://done", &json!({ "ok": false, "soft": true, "error": e }));
            }
            return;
        }
    }
    finalize_oauth_result(app, result);
}

fn deeplink_err_soft(e: &str) -> bool {
    e != "__superseded__" && e != "__attribution__"
}

fn persist_oauth_account(
    paths: &Paths,
    provider: &str,
    raw: &serde_json::Value,
    flow: &str,
    mid: &str,
    poll_ready: bool,
) -> Result<serde_json::Value, String> {
    let flow_still_ours = || {
        pending_oauth_guard()
            .as_ref()
            .map(|p| p.flow == flow)
            .unwrap_or(false)
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let jwt = raw
        .pointer("/data/token")
        .and_then(|t| t.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| i18n::tr("err.oauth.no_token"))?
        .to_string();
    let raw_access = oauth::extract_access_token(provider, raw).unwrap_or_default();
    let access_token = if provider == "zai" && !raw_access.is_empty() {
        oauth::resolve_zai_business_token(&raw_access).ok_or_else(|| i18n::tr("err.oauth.zai_business"))?
    } else {
        raw_access
    };
    let userinfo = if poll_ready {
        oauth::extract_poll_user_profile(raw)
    } else {
        oauth::extract_user_profile(provider, raw)
    }
    .or_else(|| {
        (!access_token.is_empty())
            .then(|| oauth::fetch_userinfo(provider, &access_token))
            .flatten()
    });
    let refresh_token = oauth::extract_refresh_token(provider, raw);
    let credentials = oauth::assemble_credentials_with_token(
        provider,
        &jwt,
        userinfo.as_ref(),
        (!access_token.is_empty()).then_some(access_token.as_str()),
        refresh_token.as_deref(),
    );
    let config = oauth::assemble_config(provider, &jwt, &access_token);

    let _lock = store_guard();
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    let accounts = list_accounts(paths)?;
    let hash = canonical_hash(&credentials);
    if let Some(i) = store::find_same_login(&credentials, &hash, &accounts, &paths.home) {
        let mut dup = accounts[i].clone();
        dup.hash = hash.clone();
        dup.credentials = credentials;
        dup.config = Some(config);
        dup.updated_at = now_ts();
        if dup.virtual_device_mid.as_deref().map_or(true, |m| m.trim().is_empty()) {
            dup.virtual_device_mid = Some(mid.to_string());
        }
        if !flow_still_ours() {
            return Err("__superseded__".into());
        }
        save_account(paths, &dup)?;
        *pending_oauth_guard() = None;
        return Ok(json!({ "id": dup.id, "name": dup.name, "provider": provider, "duplicate": true }));
    }
    let base = credentials
        .get(format!("oauth:{provider}:user_info"))
        .and_then(|v| v.as_str())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|u| u.get("username").and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_else(|| match provider {
            "zai" => "z.ai".to_string(),
            _ => "BigModel".to_string(),
        });
    let name = unique_name(&accounts, &base);
    let ts = now_ts();
    let acc = Account {
        id: uuid::Uuid::new_v4().to_string(),
        name: name.clone(),
        created_at: ts.clone(),
        updated_at: ts,
        hash,
        credentials,
        config: Some(config),
        virtual_device_mid: Some(mid.to_string()),
        virtual_arms_uid: Some(store::new_arms_uid()),
    };
    if !flow_still_ours() {
        return Err("__superseded__".into());
    }
    save_account(paths, &acc)?;
    *pending_oauth_guard() = None;
    Ok(json!({ "id": acc.id, "name": acc.name, "provider": provider }))
}

fn finalize_oauth_result(app: &AppHandle, result: Result<serde_json::Value, String>) {
    if let Err(e) = &result {
        if e == "__superseded__" || e == "__attribution__" {
            return;
        }
    }
    if let Some(w) = app.get_webview_window("login") {
        let _ = w.close();
    }
    let payload = match result {
        Ok(v) => v,
        Err(e) => json!({ "ok": false, "error": e }),
    };
    let _ = app.emit("oauth://done", &payload);
}

fn spawn_poll_loop(app: AppHandle, provider: String, flow: String, mid: String, cfg: PollCfg) {
    std::thread::spawn(move || {
        let ours = || pending_oauth_guard().as_ref().map(|p| p.flow == flow).unwrap_or(false);
        let deadline = {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            std::cmp::min(cfg.expires_at_ms, now + u128::from(oauth::FLOW_TIMEOUT_MS))
        };
        loop {
            if !ours() {
                flowlog::log(&flow, "poll-exit", "flow-done-or-replaced");
                return;
            }
            match oauth::poll_flow_once(&cfg.url, &cfg.token, &mid) {
                Ok(oauth::PollOutcome::Pending) => {}
                Ok(oauth::PollOutcome::Ready(data)) => {
                    flowlog::log(&flow, "poll-ready", "");
                    let raw = json!({ "code": 0, "data": data });
                    let result = persist_oauth_account(&Paths::detect(), &provider, &raw, &flow, &mid, true);
                    match &result {
                        Ok(v) => flowlog::log(
                            &flow,
                            "persist-ok",
                            &format!("channel=poll duplicate={}", v.get("duplicate").is_some()),
                        ),
                        Err(e) if e != "__superseded__" => {
                            flowlog::log(&flow, "persist-fail", &format!("channel=poll {e}"));
                        }
                        Err(_) => {}
                    }
                    finalize_oauth_result(&app, result);
                    return;
                }
                Err(e) => {
                    if !ours() {
                        flowlog::log(&flow, "poll-exit", "superseded");
                        return;
                    }
                    flowlog::log(&flow, "poll-fail", &e);
                    *pending_oauth_guard() = None;
                    finalize_oauth_result(&app, Err(e));
                    return;
                }
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            if now >= deadline {
                if !ours() {
                    return;
                }
                flowlog::log(&flow, "poll-timeout", "");
                *pending_oauth_guard() = None;
                finalize_oauth_result(&app, Err(i18n::tr("err.oauth.expired")));
                return;
            }
            let sleep = (deadline - now).min(cfg.interval_ms as u128) as u64;
            std::thread::sleep(std::time::Duration::from_millis(sleep));
        }
    });
}

fn captcha_label(claim_id: &str) -> String {
    format!("captcha-{claim_id}")
}

fn open_captcha_window(app: &AppHandle, claim_id: &str, auto: bool) -> Result<(), String> {
    let (w, h) = (380.0, 320.0);
    let label = captcha_label(claim_id);
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.eval("location.reload()");
        center_over_main(app, &win, w, h);
        if auto {
            let _ = win.hide();
        } else {
            let _ = win.show();
            let _ = win.set_focus();
        }
        return Ok(());
    }
    let win = tauri::WebviewWindowBuilder::new(
        app,
        label,
        tauri::WebviewUrl::App(format!("captcha.html?claimId={claim_id}&auto={}", u8::from(auto)).into()),
    )
    .title(i18n::tr("title.captcha"))
    .theme(Some(tauri::Theme::Dark))
    .background_color(tauri::window::Color(10, 10, 12, 255))
    .inner_size(w, h)
    .min_inner_size(340.0, 280.0)
    .maximizable(false)
    .resizable(false)
    .additional_browser_args("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-proxy-server")
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    center_over_main(app, &win, w, h);
    if !auto {
        let _ = win.show();
        let _ = win.set_focus();
    }
    Ok(())
}

#[tauri::command]
async fn kill_zcode(app: AppHandle) -> Result<(), String> {
    let _guard = store_guard();
    let r = if store::kill_zcode()? { Ok(()) } else { Err(i18n::tr("err.zcode.kill_timeout")) };
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn set_behavior(
    app: AppHandle,
    launch_after_switch: Option<bool>,
    close_to_tray: Option<bool>,
    hot_switch: Option<bool>,
    auto_claim: Option<bool>,
    activity_probe: Option<bool>,
    claim_at_effective: Option<bool>,
) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    if let Some(v) = launch_after_switch {
        s.launch_after_switch = Some(v);
    }
    if let Some(v) = close_to_tray {
        s.close_to_tray = Some(v);
    }
    if let Some(v) = hot_switch {
        s.hot_switch = Some(v);
    }
    if let Some(v) = auto_claim {
        s.auto_claim = Some(v);
    }
    if let Some(v) = activity_probe {
        s.activity_probe = Some(v);
    }
    if let Some(v) = claim_at_effective {
        s.claim_at_effective = Some(v);
    }
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn set_language(app: AppHandle, lang: String) -> Result<(), String> {
    let l = i18n::Lang::parse(&lang)
        .ok_or_else(|| i18n::trf("err.lang.unknown", &[("lang", &lang)]))?;
    {
        let _guard = store_guard();
        let paths = Paths::detect();
        let mut s = load_settings(&paths);
        s.language = Some(l.as_str().to_string());
        save_settings(&paths, &s)?;
    }
    i18n::set(l);
    rebuild_tray(&app);
    for (label, key) in [("settings", "title.settings"), ("captcha", "title.captcha"), ("login", "title.login")] {
        if let Some(w) = app.get_webview_window(label) {
            let _ = w.set_title(&i18n::tr(key));
        }
    }
    let _ = app.emit("state-changed", ());
    Ok(())
}

#[tauri::command]
async fn reveal_main(app: AppHandle) -> Result<(), String> {
    let win = app.get_webview_window("main").ok_or_else(|| i18n::tr("err.main.missing"))?;
    win.show().map_err(|e| e.to_string())?;
    let _ = win.set_focus();
    Ok(())
}

#[tauri::command]
async fn open_settings(app: AppHandle) -> Result<(), String> {
    let (w, h) = (520.0, 700.0);
    if let Some(win) = app.get_webview_window("settings") {
        center_over_main(&app, &win, w, h);
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }
    let win = tauri::WebviewWindowBuilder::new(
        &app,
        "settings",
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title(i18n::tr("title.settings"))
    .theme(Some(tauri::Theme::Dark))
    .background_color(tauri::window::Color(10, 10, 12, 255))
    .inner_size(w, h)
    .min_inner_size(440.0, 540.0)
    .resizable(true)
    .additional_browser_args("--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection --no-proxy-server")
    .visible(false)
    .build()
    .map_err(|e| e.to_string())?;
    center_over_main(&app, &win, w, h);
    let _ = win.show();
    let _ = win.set_focus();
    Ok(())
}

fn center_over_main(app: &AppHandle, win: &tauri::WebviewWindow, w: f64, h: f64) {
    let Some(m) = app.get_webview_window("main") else { return };
    let (Ok(p), Ok(s), Ok(scale)) = (m.outer_position(), m.outer_size(), m.scale_factor()) else { return };
    let (mx, my) = (p.x as f64 / scale, p.y as f64 / scale);
    let (mw, mh) = (s.width as f64 / scale, s.height as f64 / scale);
    let x = mx + (mw - w).max(0.0) / 2.0;
    let y = my + (mh - h).max(0.0) / 2.0;
    let _ = win.set_position(tauri::LogicalPosition::new(x, y));
}

#[tauri::command]
async fn autostart_status(app: AppHandle) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn autostart_set(app: AppHandle, enable: bool) -> Result<bool, String> {
    let al = app.state::<AutoLaunchManager>();
    if enable {
        al.enable().map_err(|e| e.to_string())?;
    } else {
        al.disable().map_err(|e| e.to_string())?;
    }
    al.is_enabled().map_err(|e| e.to_string())
}

#[tauri::command]
async fn export_pick_path(app: AppHandle, id: String) -> Result<serde_json::Value, String> {
    let acc = load_account(&Paths::detect(), &id)?;
    let default_name = format!("{}.zsb", sanitize_filename(&acc.name));
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.zsb"), &["zsb"])
        .set_file_name(&default_name)
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "name": acc.name }))
}

#[tauri::command]
async fn export_finalize(path: String, id: String, password: String) -> Result<serde_json::Value, String> {
    let acc = load_account(&Paths::detect(), &id)?;
    let payload = store::export_bundle_value(std::slice::from_ref(&acc));
    let sealed = cipher::seal(&payload, &password, cipher::FORMAT_BUNDLE)?;
    let body = serde_json::to_string_pretty(&sealed).unwrap() + "\n";
    store::atomic_write(std::path::Path::new(&path), &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "saved": true, "path": path }))
}

#[tauri::command]
async fn export_all_pick_path(app: AppHandle) -> Result<serde_json::Value, String> {
    let accounts = list_accounts(&Paths::detect())?;
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty"));
    }
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.zsb"), &["zsb"])
        .set_file_name("zcode-accounts.zsb")
        .blocking_save_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy(), "count": accounts.len() }))
}

#[tauri::command]
async fn export_all_finalize(path: String, password: String) -> Result<serde_json::Value, String> {
    let accounts = list_accounts(&Paths::detect())?;
    if accounts.is_empty() {
        return Err(i18n::tr("err.export.empty_short"));
    }
    let payload = store::export_bundle_value(&accounts);
    let sealed = cipher::seal(&payload, &password, cipher::FORMAT_BUNDLE)?;
    let body = serde_json::to_string_pretty(&sealed).unwrap() + "\n";
    store::atomic_write(std::path::Path::new(&path), &body).map_err(|e| i18n::trf("err.write", &[("e", &e.to_string())]))?;
    Ok(json!({ "saved": true, "path": path, "count": accounts.len() }))
}

#[tauri::command]
async fn import_pick_files(app: AppHandle) -> Result<serde_json::Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.zsb"), &["zsb"])
        .blocking_pick_files();
    let Some(files) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let mut sealed = vec![];
    let mut errors = vec![];
    for fp in files {
        let path = match fp.into_path() {
            Ok(p) => p,
            Err(e) => {
                errors.push(i18n::trf("err.path.conv", &[("e", &e.to_string())]));
                continue;
            }
        };
        let fname = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(e) => {
                errors.push(i18n::trf("err.import.read", &[("fname", fname.as_str()), ("e", &e.to_string())]));
                continue;
            }
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) => {
                if !cipher::is_sealed(&v) {
                    errors.push(i18n::trf("err.import.not_sealed", &[("fname", fname.as_str())]));
                } else if v.get("format").and_then(|f| f.as_str()) != Some(cipher::FORMAT_BUNDLE) {
                    errors.push(i18n::trf("err.import.not_bundle", &[("fname", fname.as_str())]));
                } else {
                    sealed.push((fname, v));
                }
            }
            Err(e) => errors.push(i18n::trf("err.import.json", &[("fname", fname.as_str()), ("e", &e.to_string())])),
        }
    }
    Ok(json!({ "picked": true, "sealed": sealed, "errors": errors }))
}

#[tauri::command]
async fn import_sealed(app: AppHandle, files: Vec<(String, Value)>, password: String) -> Result<ImportReport, String> {
    let _guard = store_guard();
    let mut decrypted = vec![];
    let mut errors = vec![];
    for (fname, v) in files {
        match cipher::open(&v, &password) {
            Ok(payload) => decrypted.push((fname, payload)),
            Err(e) => errors.push(i18n::trf("err.import.wrap", &[("fname", fname.as_str()), ("e", &e)])),
        }
    }
    let mut report = if decrypted.is_empty() {
        ImportReport::default()
    } else {
        import_values(&Paths::detect(), &decrypted)?
    };
    report.picked = true;
    report.errors.extend(errors);
    rebuild_tray(&app);
    Ok(report)
}

#[tauri::command]
async fn pick_zcode_path(app: AppHandle) -> Result<serde_json::Value, String> {
    let picked = app
        .dialog()
        .file()
        .add_filter(&i18n::tr("dialog.exe"), &["exe"])
        .blocking_pick_file();
    let Some(fp) = picked else {
        return Ok(json!({ "picked": false }));
    };
    let path = fp.into_path().map_err(|e| i18n::trf("err.path.invalid", &[("e", &e.to_string())]))?;
    Ok(json!({ "picked": true, "path": path.to_string_lossy() }))
}

#[tauri::command]
async fn set_zcode_path(app: AppHandle, path: String) -> Result<(), String> {
    let _guard = store_guard();
    let paths = Paths::detect();
    let mut s = load_settings(&paths);
    s.zcode_path = store::normalize_zcode_path(&path);
    let r = save_settings(&paths, &s);
    rebuild_tray(&app);
    let _ = app.emit("state-changed", ());
    r
}

#[tauri::command]
async fn launch_zcode(app: AppHandle) -> Result<(), String> {
    let paths = Paths::detect();
    let (p, ok) = effective_zcode_path(&paths);
    if !ok {
        return Err(i18n::trf("err.zcode.path_invalid_hint", &[("p", &p)]));
    }
    let r = store::launch_zcode(&p);
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn open_external(url: String) -> Result<(), String> {
    store::open_url(&url)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            show_main(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .invoke_handler(tauri::generate_handler![
            get_state,
            app_version,
            capture_current,
            rename_account,
            delete_account,
            update_account_from_live,
            switch_to,
            get_live_quota,
            get_account_quota,
            reset_device,
            claim_preview,
            claim_refresh,
            claim_current,
            claim_legacy_count,
            claim_adopt_legacy_history,
            claim_drop_legacy_history,
            claim_start,
            claim_captcha_config,
            claim_captcha_submit,
            claim_cancel,
            claim_status,
            claim_sessions,
            claim_acknowledge_unknown,
            claim_pending,
            activity_probe,
            set_activity_tip,
            tray_flash,
            tray_flash_stop,
            log_probe,
            log_client_error,
            oauth_providers,
            oauth_begin,
            set_auth_proxy,
            kill_zcode,
            set_behavior,
            set_language,
            autostart_status,
            autostart_set,
            export_pick_path,
            export_finalize,
            export_all_pick_path,
            export_all_finalize,
            import_pick_files,
            import_sealed,
            pick_zcode_path,
            set_zcode_path,
            launch_zcode,
            open_external,
            open_settings,
            reveal_main,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "main" {
                    let paths = Paths::detect();
                    if store::load_settings(&paths).close_to_tray() {
                        api.prevent_close();
                        let _ = window.hide();
                    }
                }
                if let Some(claim_id) = window.label().strip_prefix("captcha-") {
                    let claim_id = claim_id.to_string();
                    let cancelled = {
                        let mut registry = claim_registry_guard();
                        registry
                            .as_mut()
                            .map(|registry| registry.cancel(&claim_id).cancelled)
                            .unwrap_or(false)
                    };
                    if cancelled {
                        let _ = window
                            .app_handle()
                            .emit("claim://cancelled", json!({ "claimId": claim_id }));
                    }
                }
            }
        })
        .setup(|app| {
            let settings = store::load_settings(&Paths::detect());
            i18n::init_from_settings(&settings);
            net::set_proxy(settings.auth_proxy().map(str::to_string));
            if let Ok(data_dir) = app.path().app_local_data_dir() {
                flowlog::init(&data_dir);
            }
            {
                let home = Paths::detect().home;
                std::thread::spawn(move || changelog::refresh_cache(&home));
            }
            let _tray = TrayIconBuilder::with_id(TRAY_ID)
                .icon(app.default_window_icon().expect("no window icon").clone())
                .tooltip("Z·SWITCH")
                .menu(&tray_menu_inner(app.handle())?)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| {
                    let id = event.id().as_ref().to_string();
                    match id.as_str() {
                        "show" => show_main(app),
                        "activity" => {
                            TRAY_FLASH_ACTIVE.store(false, Ordering::SeqCst);
                            let _ = app.emit("tray-activity", ());
                        }
                        "quit" => app.exit(0),
                        other => run_tray_action(app.clone(), other.to_string()),
                    }
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, button_state: tauri::tray::MouseButtonState::Up, .. } = event {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[test]
fn claim_failure_result_carries_every_field_the_ui_needs() {
    let summary = claim_session::ClaimSummary {
        claim_id: "c1".into(),
        account_id: "account-a".into(),
        account_name: "Synthetic A".into(),
        revision: "rev-1".into(),
        plan_id: "p1".into(),
        plan_name: "Plan One".into(),
        status: "failed".into(),
        auto: false,
        created_at: 1,
        updated_at: 2,
        manual_retry: false,
        identity_scope: "zai:user".into(),
        result: None,
    };
    let terminal = claim::ClaimError {
        code: 1005,
        message: "quota".into(),
        next_at: Some(1_700_000_100_000),
        unknown: false,
    };
    let payload = claim_failure_result(&summary, &terminal, 900);
    assert_eq!(payload["claimId"], "c1");
    assert_eq!(payload["planId"], "p1");
    assert_eq!(payload["revision"], "rev-1");
    assert_eq!(payload["code"], 1005);
    assert_eq!(payload["terminal"], true);
    assert_eq!(payload["challenge"], false);
    assert_eq!(payload["nextAt"], 1_700_000_100_000_i64);
    assert_eq!(payload["unknown"], false);
    assert_eq!(payload["coolingSecs"], 900);

    let challenge = claim::ClaimError {
        code: claim::CLAIM_ERR_CHALLENGE,
        message: "challenge".into(),
        next_at: None,
        unknown: false,
    };
    let payload = claim_failure_result(&summary, &challenge, 0);
    assert_eq!(payload["challenge"], true);
    assert_eq!(payload["terminal"], false);
    assert!(
        payload.get("coolingSecs").is_none(),
        "没有冷却时不要塞 0 值字段"
    );

    let lost = claim::ClaimError {
        code: -1,
        message: "lost".into(),
        next_at: None,
        unknown: true,
    };
    let payload = claim_failure_result(&summary, &lost, 0);
    assert_eq!(payload["unknown"], true);
    assert_eq!(payload["terminal"], false);
}

    #[test]
    fn claim_start_guards_abort_only_on_explicit_risk() {
        let home = std::env::temp_dir().join(format!(
            "zsw-guards-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(home.join(".zcode-switch")).unwrap();
        let paths = store::Paths { home: home.clone(), data_root: home.clone() };

        // 激活上报的明确风控：中止 + 记录冷却
        let err = claim_start_guards(
            &paths,
            "account-a",
            || Err("activation HTTP 200 (code 3012): unusual activity".into()),
            || Ok(()),
        )
        .expect_err("激活风控必须中止");
        assert!(err.contains("风控") || err.contains("3012"));
        assert!(risk::is_cooling(&paths, "account-a"), "必须写入同一冷却状态");

        // 验证码配置的明确风控：同样中止 + 冷却
        let captcha_err = claim_start_guards(
            &paths,
            "account-b",
            || Ok(()),
            || Err("config HTTP 405 (code 3012): unusual activity".into()),
        )
        .expect_err("验证码配置风控必须中止");
        assert!(captcha_err.contains("风控") || captcha_err.contains("3012"));
        assert!(risk::is_cooling(&paths, "account-b"));

        // 普通网络故障保持降级：不中止、不冷却
        let normal = claim_start_guards(
            &paths,
            "account-c",
            || Err("activation network timeout".into()),
            || Err("config network timeout".into()),
        );
        assert!(normal.is_ok(), "普通网络错误不得中止领取");
        assert!(!risk::is_cooling(&paths, "account-c"));

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn legacy_adoption_requires_the_confirmed_snapshot() {
        let home = std::env::temp_dir().join(format!(
            "zsw-adopt-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(home.join(".zcode-switch")).unwrap();
        let secret = zcrypto::default_secret(&home);
        let credentials = crate::test_support::account("a", "zai").credentials;
        let identity = claim_session::identity_scope(&credentials, &secret);
        let snapshot = store::ClaimSnapshot {
            account: store::Account {
                id: "account-a".into(),
                name: "A".into(),
                created_at: String::new(),
                updated_at: String::new(),
                hash: String::new(),
                credentials: credentials.clone(),
                config: None,
                virtual_device_mid: None,
                virtual_arms_uid: None,
            },
            revision: "rev-1".into(),
            device_initialized: false,
        };
        // 与确认时一致 → 允许归属
        assert_eq!(
            legacy_adopt_identity(&snapshot, Some(&identity), None, &secret).unwrap(),
            identity
        );
        // 身份作用域不一致（账号在操作中被换成另一登录）→ 中止
        assert!(legacy_adopt_identity(&snapshot, Some("zai:other"), None, &secret).is_err());
        assert!(
            legacy_adopt_identity(&snapshot, None, None, &secret).is_err(),
            "缺少确认作用域必须中止"
        );

        // live 行：快照哈希必须匹配
        let live_creds = crate::test_support::account("b", "bigmodel").credentials;
        let live_hash = store::canonical_hash(&live_creds);
        let live_identity = claim_session::identity_scope(&live_creds, &secret);
        let live_snapshot = store::ClaimSnapshot {
            account: store::Account {
                id: store::LIVE_ID.into(),
                name: "live".into(),
                created_at: String::new(),
                updated_at: String::new(),
                hash: String::new(),
                credentials: live_creds.clone(),
                config: None,
                virtual_device_mid: None,
                virtual_arms_uid: None,
            },
            revision: "rev-live".into(),
            device_initialized: false,
        };
        assert!(legacy_adopt_identity(
            &live_snapshot,
            Some(&live_identity),
            Some(&live_hash),
            &secret
        )
        .is_ok());
        assert!(legacy_adopt_identity(
            &live_snapshot,
            Some(&live_identity),
            Some("stale-hash"),
            &secret
        )
        .is_err());
        assert!(
            legacy_adopt_identity(&live_snapshot, Some(&live_identity), None, &secret).is_err(),
            "live 缺少快照哈希必须中止"
        );

        let _ = std::fs::remove_dir_all(&home);
    }
