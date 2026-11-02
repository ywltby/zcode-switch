
use crate::quota;
#[cfg(test)]
mod tests;
use crate::zcrypto;
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

pub const BILLING_PREVIEW_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/preview";
pub const BILLING_CLAIM_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/claim";
pub const CLIENT_CONFIGS_URL: &str = "https://zcode.z.ai/api/v1/client/configs";
/// Campaign plans (the client's "可领取" banner) are delivered here, not by billing/preview.
pub const MARKETING_TOUCH_URL: &str = "https://zcode.z.ai/api/v1/marketing/touch";

const CLAIM_TIMEOUT_SECS: u64 = 25;
const MARKETING_SEQ: u32 = 1;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaimGrant {
    pub name: String,
    pub units: f64,
    pub period: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ClaimPlan {
    pub plan_id: String,
    pub name: String,
    pub description: String,
    pub priority: i64,
    pub grants: Vec<String>,
    #[serde(default)]
    pub grant_items: Vec<ClaimGrant>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptchaConfig {
    pub enabled: bool,
    pub region: String,
    pub prefix: String,
    pub scene_id: String,
}

#[derive(Debug, Clone)]
pub struct ClaimError {
    pub code: i64,
    pub message: String,
    pub next_at: Option<i64>,
    /// True when the request may have reached the server but no definitive
    /// business outcome was observed.
    pub unknown: bool,
}

impl From<String> for ClaimError {
    fn from(message: String) -> Self {
        ClaimError {
            code: -1,
            message,
            next_at: None,
            unknown: false,
        }
    }
}

fn claim_error(code: i64, body: &Value) -> ClaimError {
    let next_at = if code == 1005 {
        body.pointer("/data/plan/ends_at")
            .and_then(|x| x.as_i64())
            .map(|s| s * 1000)
    } else {
        None
    };
    ClaimError {
        code,
        message: failure_message(code, body),
        next_at,
        unknown: false,
    }
}

fn unknown_claim_error(message: String) -> ClaimError {
    ClaimError {
        code: -1,
        message,
        next_at: None,
        unknown: true,
    }
}

pub fn failure_payload(
    account_id: &str,
    account_name: &str,
    plan_name: &str,
    err: &ClaimError,
) -> Value {
    serde_json::json!({
        "ok": false,
        "accountId": account_id,
        "accountName": account_name,
        "planName": plan_name,
        "code": err.code,
        "nextAt": err.next_at,
        "message": err.message,
    })
}

/// Structured scope failure. `notStarted` is true only when no activation report,
/// preview, or claim request was sent for this attempt.
pub fn scope_payload(account_id: &str, revision: &str, not_started: bool) -> String {
    serde_json::json!({
        "code": "scope_changed",
        "accountId": account_id,
        "revision": revision,
        "notStarted": not_started,
    })
    .to_string()
}

fn claim_token(creds: &Value, _config: Option<&Value>, secret: &str) -> Result<String, String> {
    crate::quota_auth::strict_zcode_jwt(creds, secret).map_err(|_| crate::i18n::tr("err.claim.no_jwt"))
}

fn decrypt_credential(v: &str, secret: &str) -> Option<String> {
    if zcrypto::is_encrypted(v) {
        zcrypto::decrypt_with_secret(v, secret).ok()
    } else {
        Some(v.to_string())
    }
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(CLAIM_TIMEOUT_SECS))
        .build()
}

pub const EVENT_REPORT_URL: &str = "https://zcode.z.ai/api/v1/event/report";
pub const ACTIVATION_EVENTS: [&str; 2] = ["app_launch", "app_daily_active"];
const SCREEN_RESOLUTION: &str = "2560x1440";
const ACTIVATION_TIMEOUT_SECS: u64 = 10;

pub(crate) fn telemetry_user_id(home: &Path, creds: &Value) -> Option<String> {
    let secret = zcrypto::default_secret(home);
    let provider = creds
        .get("oauth:active_provider")
        .and_then(|v| v.as_str())
        .and_then(|v| decrypt_credential(v, &secret))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "zai".to_string());
    let raw = creds
        .get(format!("oauth:{provider}:user_info"))?
        .as_str()?;
    let plain = decrypt_credential(raw, &secret)?;
    let info: Value = serde_json::from_str(&plain).ok()?;
    let pick = |k: &str| {
        info.get(k)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    pick("id").or_else(|| pick("user_id")).map(String::from)
}

fn activation_event_body(element: &str, event_id: &str, user_id: &str, mid: &str) -> Value {
    serde_json::json!({
        "event_id": event_id,
        "client_timezone": quota::client_timezone(),
        "client_language": quota::ZCODE_LANG,
        "element_name": element,
        "event_region": "app",
        "event_type": "view",
        "event_text": "",
        "event_extra_detail": {},
        "user_id": user_id,
        "screen_resolution": SCREEN_RESOLUTION,
        "app_version": quota::zcode_app_version(),
        "device_os_category": device_os_category(),
        "device_os_version": quota::os_version().unwrap_or_default(),
        "device_mid": mid,
        "mac_id": "",
        "marketing_params": "{}",
    })
}

fn device_os_category() -> &'static str {
    match std::env::consts::OS {
        "windows" => "windows",
        "macos" => "macos",
        _ => "linux",
    }
}

pub fn report_activation_events(user_id: &str, device_mid: &str) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(ACTIVATION_TIMEOUT_SECS))
        .build();
    for element in ACTIVATION_EVENTS {
        let body = activation_event_body(
            element,
            &uuid::Uuid::new_v4().to_string(),
            user_id,
            device_mid,
        );
        let resp = agent
            .post(EVENT_REPORT_URL)
            .set("Content-Type", "application/json")
            .send_json(body)
            .map_err(|e| {
                crate::i18n::trf("err.claim.activate_req", &[("e", http_err("", e).trim())])
            })?
            .into_string()
            .map_err(|e| crate::i18n::trf("err.claim.activate_req", &[("e", &e.to_string())]))?;
        let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
        let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
        if code != 0 {
            return Err(failure_message(code, &v));
        }
    }
    Ok(())
}

/// Claimable plans from every source the official client uses: the billing preview
/// and the marketing campaign that drives the in-app "可领取" banner.
pub fn claim_plans(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    device_mid: Option<String>,
) -> Result<Vec<ClaimPlan>, String> {
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let mut plans: Vec<ClaimPlan> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    match preview_once(&preview_url(), &token, device_mid.clone()) {
        Ok(list) => merge_plans(list, &mut plans),
        Err(e) => errors.push(e),
    }
    match marketing_plans(&token, device_mid) {
        Ok(list) => merge_plans(list, &mut plans),
        Err(e) => errors.push(e),
    }
    plans.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.plan_id.cmp(&b.plan_id)));
    if plans.is_empty() && !errors.is_empty() {
        return Err(errors.join(" · "));
    }
    Ok(plans)
}

fn merge_plans(incoming: Vec<ClaimPlan>, plans: &mut Vec<ClaimPlan>) {
    for plan in incoming {
        if !plans.iter().any(|p| p.plan_id == plan.plan_id) {
            plans.push(plan);
        }
    }
}

fn preview_url() -> String {
    format!(
        "{BILLING_PREVIEW_URL}?app_version={}&platform={}",
        quota::zcode_app_version(),
        quota::client_platform()
    )
}

fn marketing_plans(token: &str, mid: Option<String>) -> Result<Vec<ClaimPlan>, String> {
    let url = format!("{MARKETING_TOUCH_URL}?seq={MARKETING_SEQ}");
    let mut req = agent().get(&url);
    for (k, v) in quota::zai_billing_headers_with_mid(token, mid) {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| http_err(&crate::i18n::tr("err.claim.touch_req"), e))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(failure_message(code, &v));
    }
    Ok(parse_marketing_plans(&v))
}

/// Campaign deliveries carry `claim_zcode_plan` buttons plus the plan body that the
/// banner renders. Both live under `banner` / `popup`, so both slots are scanned.
fn parse_marketing_plans(body: &Value) -> Vec<ClaimPlan> {
    let mut out: Vec<ClaimPlan> = Vec::new();
    let Some(deliveries) = body.pointer("/data/deliveries").and_then(Value::as_array) else {
        return out;
    };
    for delivery in deliveries {
        for slot in ["banner", "popup"] {
            let Some(node) = delivery.get(slot).filter(|n| n.is_object()) else {
                continue;
            };
            let buttons = node.get("buttons").and_then(Value::as_array);
            for button in buttons.into_iter().flatten() {
                let action = button.get("action");
                if action.and_then(|a| a.get("type")).and_then(Value::as_str)
                    != Some("claim_zcode_plan")
                {
                    continue;
                }
                let Some(plan_id) = action
                    .and_then(|a| a.pointer("/args/plan_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                if out.iter().any(|p| p.plan_id == plan_id) {
                    continue;
                }
                let detail = [
                    node.pointer("/background/args/zcode_plan"),
                    node.pointer("/success_popup/hero/args/zcode_plan"),
                    node.pointer("/hero/args/zcode_plan"),
                ]
                .into_iter()
                .flatten()
                .find(|plan| plan.get("plan_id").and_then(Value::as_str) == Some(plan_id));
                out.push(match detail.and_then(parse_plan) {
                    Some(plan) => plan,
                    None => ClaimPlan {
                        plan_id: plan_id.to_string(),
                        name: plan_id.to_string(),
                        description: String::new(),
                        priority: 0,
                        grants: Vec::new(),
                        grant_items: Vec::new(),
                    },
                });
            }
        }
    }
    out
}

fn http_err(prefix: &str, e: ureq::Error) -> String {
    if let ureq::Error::Status(code, resp) = e {
        let body = resp.into_string().unwrap_or_default();
        let msg = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| ["msg", "message", "error"]
                .iter()
                .find_map(|k| v.get(k).and_then(|x| x.as_str()).map(String::from)))
            .unwrap_or_default();
        return format!("{prefix} HTTP {code}: {msg}");
    }
    format!("{prefix} {e}")
}

fn preview_once(url: &str, token: &str, mid: Option<String>) -> Result<Vec<ClaimPlan>, String> {
    let mut req = agent().get(url);
    for (k, v) in quota::zai_billing_headers_with_mid(token, mid) {
        req = req.set(&k, &v);
    }
    let resp = req
        .call()
        .map_err(|e| http_err(&crate::i18n::tr("err.claim.preview_req"), e))?
        .into_string()
        .map_err(|e| crate::i18n::trf("err.http.read", &[("e", &e.to_string())]))?;
    let v: Value = serde_json::from_str(&resp).unwrap_or(Value::String(resp));
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(failure_message(code, &v));
    }
    let plans = v
        .pointer("/data/plans")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out: Vec<ClaimPlan> = plans.iter().filter_map(parse_plan).collect();
    out.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.plan_id.cmp(&b.plan_id)));
    Ok(out)
}

pub fn submit_claim(
    home: &Path,
    creds: &Value,
    config: Option<&Value>,
    plan_id: &str,
    captcha_param: &str,
    captcha_region: Option<&str>,
    device_mid: Option<String>,
) -> Result<Value, ClaimError> {
    if captcha_param.trim().is_empty() {
        return Err(crate::i18n::tr("err.claim.no_captcha").into());
    }
    let secret = zcrypto::default_secret(home);
    let token = claim_token(creds, config, &secret)?;
    let mut req = agent().post(BILLING_CLAIM_URL);
    for (k, v) in quota::zai_billing_headers_with_mid(&token, device_mid) {
        req = req.set(&k, &v);
    }
    req = req.set("X-Aliyun-Captcha-Verify-Param", captcha_param.trim());
    if let Some(r) = captcha_region.filter(|r| !r.trim().is_empty()) {
        req = req.set("X-Aliyun-Captcha-Verify-Region", r.trim());
    }
    let sent = req.send_json(serde_json::json!({ "plan_id": plan_id }));
    let response = match sent {
        Ok(response) => response,
        // A definitive client rejection is a failed claim; anything else means the
        // POST may have reached the server and the result is unknown.
        Err(ureq::Error::Status(code, response)) => {
            let message = http_err(
                &crate::i18n::tr("err.claim.claim_req"),
                ureq::Error::Status(code, response),
            );
            if (400..500).contains(&code) {
                return Err(ClaimError { code: -1, message, next_at: None, unknown: false });
            }
            return Err(unknown_claim_error(message));
        }
        Err(error) => {
            return Err(unknown_claim_error(http_err(
                &crate::i18n::tr("err.claim.claim_req"),
                error,
            )))
        }
    };
    let resp = response.into_string().map_err(|error| {
        unknown_claim_error(crate::i18n::trf("err.http.read", &[("e", &error.to_string())]))
    })?;
    let v: Value = match serde_json::from_str(&resp) {
        Ok(value) => value,
        Err(_) => return Err(unknown_claim_error(crate::i18n::tr("err.claim.claim_req"))),
    };
    let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(claim_error(code, &v));
    }
    Ok(v)
}

pub fn fetch_captcha_config() -> Result<CaptchaConfig, String> {
    static CACHE: std::sync::OnceLock<CaptchaCache> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(CaptchaCache::new);
    let now = chrono::Utc::now().timestamp_millis();
    cache.fetch(CLIENT_CONFIGS_URL, &fetch_config_http, now)
}

pub(crate) const CAPTCHA_CONFIG_TTL_MS: i64 = 300_000;

#[derive(Debug, PartialEq)]
pub(crate) enum CaptchaFetchError {
    Transient,
    Rejected,
}

type CaptchaHttp<'a> = dyn Fn(&str) -> Result<(u16, Value), CaptchaFetchError> + 'a;

pub(crate) struct CaptchaCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, (i64, CaptchaConfig)>>,
    fetch_lock: std::sync::Mutex<()>,
}

impl CaptchaCache {
    pub(crate) fn new() -> Self {
        Self {
            entries: std::sync::Mutex::new(std::collections::HashMap::new()),
            fetch_lock: std::sync::Mutex::new(()),
        }
    }

    /// Shared, endpoint-scoped cache. Only a usable config is cached; failures,
    /// disabled configs, and missing scene ids are fetched again next time.
    pub(crate) fn fetch(
        &self,
        endpoint: &str,
        http: &CaptchaHttp,
        now: i64,
    ) -> Result<CaptchaConfig, String> {
        if let Some(config) = self.cached(endpoint, now) {
            return Ok(config);
        }
        let _fetch = self.fetch_lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(config) = self.cached(endpoint, now) {
            return Ok(config);
        }
        let mut attempt = 0;
        loop {
            attempt += 1;
            match http(endpoint) {
                Ok((status, body)) => {
                    if status == 401 || status == 429 {
                        return Err(crate::i18n::tr("err.claim.config_unavailable"));
                    }
                    if status >= 400 {
                        if status >= 500 && attempt == 1 {
                            std::thread::sleep(Duration::from_millis(300));
                            continue;
                        }
                        return Err(crate::i18n::trf("err.claim.config_req", &[("e", &format!("HTTP {status}"))]));
                    }
                    let config = parse_captcha_config(&body)?;
                    if config.enabled && !config.scene_id.trim().is_empty() {
                        self.entries
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(endpoint.to_string(), (now, config.clone()));
                    }
                    return Ok(config);
                }
                Err(CaptchaFetchError::Transient) if attempt == 1 => {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                Err(error) => {
                    let detail = if error == CaptchaFetchError::Transient { "transient" } else { "rejected" };
                    return Err(crate::i18n::trf("err.claim.config_req", &[("e", detail)]));
                }
            }
        }
    }

    fn cached(&self, endpoint: &str, now: i64) -> Option<CaptchaConfig> {
        let entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let (at, config) = entries.get(endpoint)?;
        (now.saturating_sub(*at) < CAPTCHA_CONFIG_TTL_MS).then(|| config.clone())
    }
}

fn parse_captcha_config(v: &Value) -> Result<CaptchaConfig, String> {
    if v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1) != 0 {
        return Err(crate::i18n::tr("err.claim.config_unavailable"));
    }
    let c = v.pointer("/data/configs/captcha").cloned().unwrap_or(Value::Null);
    let s = |k: &str| c.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
    Ok(CaptchaConfig {
        enabled: c.get("enabled").and_then(|x| x.as_bool()).unwrap_or(false),
        region: s("region"),
        prefix: s("prefix"),
        scene_id: s("sceneId"),
    })
}

fn fetch_config_http(url: &str) -> Result<(u16, Value), CaptchaFetchError> {
    let mut req = agent().get(url);
    for (k, v) in quota::zai_billing_headers("") {
        req = req.set(&k, &v);
    }
    match req.call() {
        Ok(response) => {
            let status = response.status();
            let text = response
                .into_string()
                .map_err(|_| CaptchaFetchError::Transient)?;
            let value = serde_json::from_str(&text).map_err(|_| CaptchaFetchError::Rejected)?;
            Ok((status, value))
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            let value = serde_json::from_str(&text).unwrap_or(Value::Null);
            Ok((status, value))
        }
        Err(_) => Err(CaptchaFetchError::Transient),
    }
}

fn str_field<'a>(e: &'a Value, snake: &str, camel: &str) -> Option<&'a str> {
    e.get(snake)
        .and_then(|v| v.as_str())
        .or_else(|| e.get(camel).and_then(|v| v.as_str()))
}

fn num_field(e: &Value, snake: &str, camel: &str) -> Option<f64> {
    e.get(snake)
        .and_then(|v| v.as_f64())
        .or_else(|| e.get(camel).and_then(|v| v.as_f64()))
}

fn parse_plan(p: &Value) -> Option<ClaimPlan> {
    let plan_id = str_field(p, "plan_id", "planId")?.trim().to_string();
    if plan_id.is_empty() {
        return None;
    }
    let grant_items: Vec<ClaimGrant> = p
        .get("entitlements")
        .and_then(|e| e.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    str_field(e, "meter", "meter") == Some("model_usage")
                        && str_field(e, "unit_type", "unitType") == Some("token")
                        && str_field(e, "show_name", "showName")
                            .map(|s| !s.trim().is_empty())
                            .unwrap_or(false)
                })
                .map(|e| ClaimGrant {
                    name: str_field(e, "show_name", "showName").unwrap_or("").to_string(),
                    units: num_field(e, "grant_units", "grantUnits").unwrap_or(0.0),
                    period: str_field(e, "period", "period").unwrap_or("one_time").to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    let grants = grant_items
        .iter()
        .map(|g| {
            let period_cn = match g.period.as_str() {
                "daily" => "每日",
                "weekly" => "每周",
                "monthly" => "每月",
                _ => "一次性",
            };
            format!("{} · {} Token（{period_cn}）", g.name, fmt_units(g.units))
        })
        .collect();
    Some(ClaimPlan {
        name: p.get("name").and_then(|s| s.as_str()).unwrap_or("").trim().to_string(),
        description: p
            .get("description")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_string(),
        priority: p.get("priority").and_then(|x| x.as_i64()).unwrap_or(0),
        plan_id,
        grants,
        grant_items,
    })
}

fn fmt_units(n: f64) -> String {
    let trim = |x: f64| {
        let r = (x * 10.0).round() / 10.0;
        if (r - r.trunc()).abs() < f64::EPSILON {
            format!("{}", r.trunc() as i64)
        } else {
            format!("{r:.1}")
        }
    };
    if n >= 1e8 {
        format!("{}亿", trim(n / 1e8))
    } else if n >= 1e4 {
        format!("{}万", trim(n / 1e4))
    } else {
        format!("{}", n.round() as i64)
    }
}

pub fn failure_message(code: i64, body: &Value) -> String {
    let server_msg = ["msg", "message"]
        .iter()
        .find_map(|k| body.get(k).and_then(|x| x.as_str()).map(String::from))
        .unwrap_or_default();
    let base = crate::i18n::tr(match code {
        1001 => "claim.fail.1001",
        1002 => "claim.fail.1002",
        1003 => "claim.fail.1003",
        1004 => "claim.fail.1004",
        1005 => "claim.fail.1005",
        3001 => "claim.fail.3001",
        3007 => "claim.fail.3007",
        401 => "claim.fail.401",
        _ => "claim.fail.generic",
    });
    if server_msg.is_empty() {
        base
    } else {
        crate::i18n::trf("claim.fail.with_server", &[("base", &base), ("server_msg", &server_msg)])
    }
}
