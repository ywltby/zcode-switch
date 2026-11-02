pub mod cipher;
pub mod cli;
pub mod i18n;
mod claim;
mod claim_session;
mod flowlog;
mod oauth;
mod probe;
mod quota;
mod quota_auth;
mod quota_http;
mod project_token;
mod store;
mod zcrypto;
#[cfg(test)]
mod test_support;

use serde_json::{json, Value};
use std::sync::Mutex;
use store::*;
use tauri::menu::{MenuBuilder, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_autostart::AutoLaunchManager;
use tauri_plugin_dialog::DialogExt;

const TRAY_ID: &str = "main";

static STORE_LOCK: Mutex<()> = Mutex::new(());

static CLAIM_REGISTRY: Mutex<Option<claim_session::ClaimRegistry>> = Mutex::new(None);

fn claim_registry_guard() -> std::sync::MutexGuard<'static, Option<claim_session::ClaimRegistry>> {
    let mut guard = match CLAIM_REGISTRY.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        let paths = Paths::detect();
        *guard = Some(claim_session::ClaimRegistry::load(
            claim_session::attempts_path(&paths.store_dir()),
        ));
    }
    guard
}

/// Claim snapshots cover two identities: a stored account (which owns a virtual
/// device id) and the login ZCode runs right now (`live`, real device id, nothing
/// written to disk).
fn claim_snapshot(paths: &Paths, id: &str, expected: Option<&str>) -> Result<ClaimSnapshot, String> {
    if id != store::LIVE_ID {
        return store::claim_snapshot_locked(paths, id, expected);
    }
    let mut account = store::live_claim_snapshot(paths)?;
    account.name = zcrypto::account_identity(&account.credentials, &paths.home)
        .label()
        .unwrap_or_else(|| i18n::tr("claim.live_name"));
    let revision = crate::quota_auth::quota_revision(&account, &zcrypto::default_secret(&paths.home));
    if expected.is_some_and(|value| value != revision) {
        return Err("scope_changed".into());
    }
    Ok(ClaimSnapshot { account, revision, device_initialized: false })
}

/// Re-reading must not revive a claim prepared for a different login.
fn claim_revision_ok(paths: &Paths, id: &str, revision: &str) -> bool {
    if id != store::LIVE_ID {
        return store::claim_revision_matches(paths, id, revision);
    }
    store::live_claim_snapshot(paths)
        .map(|account| {
            crate::quota_auth::quota_revision(&account, &zcrypto::default_secret(&paths.home)) == revision
        })
        .unwrap_or(false)
}

fn claim_now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
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

    let b = MenuBuilder::new(app)
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
            let _ = tray.set_tooltip(Some(&tip));
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
    let r = tauri::async_runtime::spawn_blocking(move || {
        let paths = Paths::detect();
        let mut result = {
            let _guard = store_guard();
            let hot = load_settings(&paths).hot_switch();
            store::switch_to(&paths, &id, force, restart, hot)?
        };
        result.probe = store::run_switch_probe(&paths, &id);
        Ok::<_, String>(result)
    }).await.map_err(|_| "quota_worker_failed".to_string())?;
    rebuild_tray(&app);
    r
}

#[tauri::command]
async fn get_live_quota() -> Result<quota::QuotaOverview, String> {
    tauri::async_runtime::spawn_blocking(|| store::live_quota(&Paths::detect()))
        .await.map_err(|_| "quota_worker_failed".to_string())?
}

#[tauri::command]
async fn get_account_quota(id: String, revision: Option<String>) -> Result<quota::QuotaOverview, String> {
    tauri::async_runtime::spawn_blocking(move || {
        store::account_quota_revision(&Paths::detect(), &id, revision.as_deref())
    }).await.map_err(|_| "quota_worker_failed".to_string())?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimPreviewResult {
    account_id: String,
    revision: String,
    plans: Vec<claim::ClaimPlan>,
}

#[tauri::command]
async fn claim_preview(id: String, revision: Option<String>) -> Result<ClaimPreviewResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = Paths::detect();
        let snapshot = {
            let _guard = store_guard();
        claim_snapshot(&paths, &id, revision.as_deref())?
        };
        if snapshot.device_initialized && revision.is_some() {
            // The device id changed the account revision before any HTTP request.
            return Err(claim::scope_payload(&id, &snapshot.revision, true));
        }
        let plans = claim::claim_plans(
            &paths.home,
            &snapshot.account.credentials,
            snapshot.account.config.as_ref(),
            snapshot.account.virtual_device_mid.clone(),
        )?;
        let unchanged = {
            let _guard = store_guard();
            claim_revision_ok(&paths, &id, &snapshot.revision)
        };
        if !unchanged {
            return Err(claim::scope_payload(&id, &snapshot.revision, true));
        }
        Ok(ClaimPreviewResult { account_id: id, revision: snapshot.revision, plans })
    })
    .await
    .map_err(|_| "quota_worker_failed".to_string())?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimRefreshResult {
    account_id: String,
    revision: String,
    plans: Vec<claim::ClaimPlan>,
    activated: bool,
    activation_error: Option<String>,
}

#[tauri::command]
async fn claim_refresh(id: String, revision: Option<String>) -> Result<ClaimRefreshResult, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let paths = Paths::detect();
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
                    Err(e) => (false, Some(e)),
                },
                None => (false, None),
            };
        let plans = claim::claim_plans(
            &paths.home,
            &snapshot.account.credentials,
            snapshot.account.config.as_ref(),
            snapshot.account.virtual_device_mid.clone(),
        )?;
        let unchanged = {
            let _guard = store_guard();
            claim_revision_ok(&paths, &id, &snapshot.revision)
        };
        if !unchanged {
            // The activation report was already sent, so this must not auto-retry.
            return Err(claim::scope_payload(&id, &snapshot.revision, false));
        }
        Ok(ClaimRefreshResult {
            account_id: id,
            revision: snapshot.revision,
            plans,
            activated,
            activation_error,
        })
    })
    .await
    .map_err(|_| "quota_worker_failed".to_string())?
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
    // Campaign eligibility follows the activation events the client reports on launch.
    if let Some(mid) = snapshot.account.virtual_device_mid.as_deref() {
        activate_once(&paths.home, &snapshot.account.credentials, mid);
    }
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
        }
    }

    let home = paths.home.clone();
    let credentials = snapshot.account.credentials.clone();
    let config = snapshot.account.config.clone();
    let mid = snapshot.account.virtual_device_mid.clone();
    let wanted = plan_id.clone();
    let prepared = tauri::async_runtime::spawn_blocking(move || {
        claim::claim_plans(&home, &credentials, config.as_ref(), mid)
    })
    .await;
    // A campaign read only supplies the display name: the server decides whether this
    // identity may claim the plan right now, so an empty or failed read must not block
    // a claim the user already clicked. The session stays owned by this claim either way.
    let display = claim::claim_display_name(prepared.unwrap_or_else(|_| Err(String::new())), &wanted);
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

/// The client reports activation events on every launch, which is what makes an
/// account eligible for campaigns. Mirror that once per login and device.
fn activate_once(home: &std::path::Path, credentials: &Value, device_mid: &str) {
    static REPORTED: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let Some(user_id) = claim::telemetry_user_id(home, credentials) else {
        return;
    };
    let key = format!("{user_id}:{device_mid}");
    {
        let mut done = REPORTED.lock().unwrap_or_else(|e| e.into_inner());
        if done.iter().any(|k| k == &key) {
            return;
        }
        done.push(key);
    }
    let _ = claim::report_activation_events(&user_id, device_mid);
}

#[tauri::command]
async fn claim_captcha_config() -> Result<claim::CaptchaConfig, String> {
    tauri::async_runtime::spawn_blocking(claim::fetch_captcha_config)
        .await
        .map_err(|_| "quota_worker_failed".to_string())?
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
    let submitted = tauri::async_runtime::spawn_blocking(move || {
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
    .await;

    let payload = match submitted {
        Ok(Ok(value)) => {
            let ms = |k: &str| -> Option<i64> {
                value
                    .pointer(&format!("/data/plan/{k}"))
                    .and_then(quota::timestamp_millis)
            };
            let server_time = value
                .pointer("/data/server_time")
                .and_then(quota::timestamp_millis);
            let entitlement_starts = value.pointer("/data/plan")
                .map(quota::entitlement_start_times).unwrap_or_default();
            let payload = json!({
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
            finish_claim(&claim_id, claim_session::ClaimPhase::Succeeded, Some(payload.clone()));
            payload
        }
        Ok(Err(error)) => {
            let mut payload = claim::failure_payload(
                &summary.account_id,
                &summary.account_name,
                &summary.plan_name,
                &error,
            );
            if let Some(object) = payload.as_object_mut() {
                object.insert("claimId".into(), json!(claim_id));
                object.insert("planId".into(), json!(summary.plan_id));
                object.insert("revision".into(), json!(summary.revision));
                object.insert("unknown".into(), json!(error.unknown));
            }
            let phase = if error.unknown {
                claim_session::ClaimPhase::Unknown
            } else {
                claim_session::ClaimPhase::Failed
            };
            finish_claim(&claim_id, phase, Some(payload.clone()));
            payload
        }
        Err(_) => {
            let payload = json!({
                "ok": false,
                "claimId": claim_id,
                "accountId": summary.account_id,
                "accountName": summary.account_name,
                "revision": summary.revision,
                "planId": summary.plan_id,
                "planName": summary.plan_name,
                "code": -1,
                "message": i18n::tr("err.claim.unknown"),
                "unknown": true,
            });
            finish_claim(&claim_id, claim_session::ClaimPhase::Unknown, Some(payload.clone()));
            payload
        }
    };
    close_captcha_window(&app, &claim_id);
    let _ = app.emit("claim://result", &payload);
    Ok(payload)
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

fn close_captcha_window(app: &AppHandle, claim_id: &str) {
    if let Some(w) = app.get_webview_window(&captcha_label(claim_id)) {
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
        flowlog::log(
            &flow,
            "init-ok",
            &format!("server_flow={srv_flow} expires_in={expires_in}s interval={}ms", init.poll_interval_ms),
        );
    }
    let url = init.authorize_url.clone();
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
    let mut builder = tauri::WebviewWindowBuilder::new(
        &app,
        "login",
        tauri::WebviewUrl::External(url.parse::<tauri::Url>().map_err(|e| i18n::trf("err.oauth.bad_authorize_url", &[("e", &e.to_string())]))?),
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
    builder
    .on_navigation(move |url| {
        if url.scheme() != "zcode" {
            return true;
        }
        let full = url.to_string();
        let (app3, p3, s3, f3, m3) = (app2.clone(), provider2.clone(), state2.clone(), flow2.clone(), mid2.clone());
        tauri::async_runtime::spawn(async move {
            finish_oauth(&app3, p3, s3, f3, m3, &full).await;
        });
        false
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
    spawn_poll_loop(app.clone(), provider.clone(), flow.clone(), mid, poll_cfg);
    Ok(json!({ "opened": true, "provider": provider }))
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
        virtual_arms_uid: None,
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
    for (label, window) in app.webview_windows() {
        let key = if label == "settings" {
            "title.settings"
        } else if label == "login" {
            "title.login"
        } else if label.starts_with("captcha-") {
            "title.captcha"
        } else {
            continue;
        };
        let _ = window.set_title(&i18n::tr(key));
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
            claim_preview,
            claim_refresh,
            claim_start,
            claim_captcha_config,
            claim_captcha_submit,
            claim_cancel,
            claim_status,
            claim_sessions,
            claim_acknowledge_unknown,
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
            i18n::init_from_settings(&store::load_settings(&Paths::detect()));
            if let Ok(data_dir) = app.path().app_local_data_dir() {
                flowlog::init(&data_dir);
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
