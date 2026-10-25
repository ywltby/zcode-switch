use crate::project_token::{self, Project, ProjectEndpoint, TokenService};
use crate::quota_auth::{AccountQueryContext, PlanScope, QuotaError};
use crate::quota_http::{Request, Transport};
use serde_json::Value;

#[cfg(test)]
mod tests;

#[cfg(windows)]
fn no_window(prog: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new(prog);
    c.creation_flags(0x0800_0000);
    c
}
#[cfg(not(windows))]
fn no_window(prog: &str) -> std::process::Command {
    std::process::Command::new(prog)
}

pub const BILLING_BALANCE_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/billing/balance";
pub const CLIENT_APP_VERSION: &str = "3.11.2";

pub(crate) fn client_platform() -> String {
    let os = crate::zcrypto::node_platform_for(std::env::consts::OS);
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    format!("{os}-{arch}")
}

pub const BUILTIN_CODING_PLAN_MODELS: &[&str] = &["GLM-5.3", "GLM-5.3-Flash"];
pub const BUILTIN_START_PLAN_MODELS: &[&str] = &["GLM-5.3-Flash", "GLM-5.2", "GLM-5-Turbo"];

pub(crate) fn canonical_model_id(id: &str) -> String {
    let lower = id.to_lowercase();
    for m in BUILTIN_CODING_PLAN_MODELS
        .iter()
        .chain(BUILTIN_START_PLAN_MODELS.iter())
    {
        if m.to_lowercase() == lower {
            return m.to_string();
        }
    }
    id.to_string()
}

const ZCODE_ORIGIN: &str = "https://zcode.z.ai";
pub(crate) const ZCODE_LANG: &str = "zh-CN";
const ZCODE_CHANNEL: &str = "stable";

pub(crate) fn device_mid() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let home = crate::store::pick_home(
                std::env::var("ZCODE_SWITCH_HOME")
                    .ok()
                    .map(std::path::PathBuf::from),
                std::env::var("USERPROFILE")
                    .ok()
                    .map(std::path::PathBuf::from),
                std::env::var("HOME").ok().map(std::path::PathBuf::from),
            );
            let p = crate::store::resolve_data_root(&home)
                .join(".zcode")
                .join("v2")
                .join("telemetry-state.json");
            std::fs::read_to_string(p)
                .ok()
                .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                .and_then(|v| {
                    v.get("deviceMid")
                        .and_then(|m| m.as_str())
                        .map(String::from)
                })
        })
        .clone()
}

#[cfg(windows)]
pub(crate) fn os_version() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let out = no_window("reg")
                .args([
                    "query",
                    r"HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion",
                    "/v",
                    "CurrentBuildNumber",
                ])
                .output()
                .ok()?;
            let txt = String::from_utf8_lossy(&out.stdout);
            let build = txt
                .lines()
                .find(|l| l.contains("CurrentBuildNumber"))?
                .rsplit(' ')
                .find(|t| !t.is_empty())?
                .to_string();
            Some(format!("10.0.{build}"))
        })
        .clone()
}

#[cfg(not(windows))]
pub(crate) fn os_version() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let out = no_window("uname").arg("-r").output().ok()?;
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            (!v.is_empty()).then_some(v)
        })
        .clone()
}

#[cfg(windows)]
pub(crate) fn client_timezone() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let out = no_window("tzutil").arg("/g").output().ok();
            let name = out
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            match name.as_str() {
                "China Standard Time" | "China Daylight Time" => "Asia/Shanghai",
                "Singapore Standard Time" => "Asia/Singapore",
                "Tokyo Standard Time" => "Asia/Tokyo",
                "UTC" => "UTC",
                _ => "unknown",
            }
            .to_string()
        })
        .clone()
}

#[cfg(not(windows))]
pub(crate) fn client_timezone() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "unknown".to_string())
}

pub(crate) fn zcode_app_version() -> String {
    static CACHE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            for hive in [
                r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
                r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
                r"HKCU\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall",
            ] {
                let Ok(out) = no_window("reg").args(["query", hive, "/s"]).output() else {
                    continue;
                };
                let txt = String::from_utf8_lossy(&out.stdout);
                let (mut name, mut ver) = (String::new(), String::new());
                for line in txt.lines() {
                    let l = line.trim();
                    if l.starts_with("HKEY_") {
                        if is_zcode_display_name(&name) && !ver.is_empty() {
                            return normalize_version(&ver);
                        }
                        name.clear();
                        ver.clear();
                        continue;
                    }
                    if let Some(rest) = l.strip_prefix("DisplayName") {
                        name = rest
                            .trim_start()
                            .trim_start_matches("REG_SZ")
                            .trim()
                            .to_string();
                    } else if let Some(rest) = l.strip_prefix("DisplayVersion") {
                        ver = rest
                            .trim_start()
                            .trim_start_matches("REG_SZ")
                            .trim()
                            .to_string();
                    }
                }
                if is_zcode_display_name(&name) && !ver.is_empty() {
                    return normalize_version(&ver);
                }
            }
            CLIENT_APP_VERSION.to_string()
        })
        .clone()
}

fn is_zcode_display_name(name: &str) -> bool {
    let l = name.to_lowercase();
    l.contains("zcode") && !l.contains("switch")
}

fn normalize_version(v: &str) -> String {
    let parts: Vec<&str> = v.split('.').collect();
    if parts.len() >= 3 {
        format!("{}.{}.{}", parts[0], parts[1], parts[2])
    } else {
        v.to_string()
    }
}

pub(crate) fn zai_billing_headers(token: &str) -> Vec<(String, String)> {
    zai_billing_headers_with_mid(token, device_mid())
}

pub(crate) fn zai_billing_headers_with_mid(
    token: &str,
    mid: Option<String>,
) -> Vec<(String, String)> {
    zai_headers_with_version(zcode_app_version(), token, mid)
}

pub(crate) struct OAuthFlowHeaders(pub Vec<(String, String)>);

pub(crate) fn zai_oauth_headers_with_mid(token: &str, mid: Option<String>) -> OAuthFlowHeaders {
    OAuthFlowHeaders(zai_headers_with_version(
        CLIENT_APP_VERSION.to_string(),
        token,
        mid,
    ))
}

fn zai_headers_with_version(
    ver: String,
    token: &str,
    mid: Option<String>,
) -> Vec<(String, String)> {
    let mut h: Vec<(String, String)> = vec![
        ("User-Agent".into(), format!("ZCode/{ver}")),
        ("HTTP-Referer".into(), ZCODE_ORIGIN.into()),
        ("X-Title".into(), "Z Code@electron".into()),
        ("X-ZCode-App-Version".into(), ver.clone()),
        ("X-Platform".into(), client_platform()),
        ("X-Release-Channel".into(), ZCODE_CHANNEL.into()),
        ("X-Client-Language".into(), ZCODE_LANG.into()),
        ("X-Client-Timezone".into(), client_timezone()),
        ("X-Os-Category".into(), std::env::consts::OS.into()),
    ];
    if let Some(v) = os_version() {
        h.push(("X-Os-Version".into(), v));
    }
    if let Some(mid) = mid {
        h.push(("X-Device-Mid".into(), mid));
    }
    h.push(("Authorization".into(), format!("Bearer {token}")));
    h.push(("x-request-id".into(), uuid::Uuid::new_v4().to_string()));
    h
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct QuotaItem {
    #[serde(skip_serializing)]
    pub window_key: Option<String>,
    pub name: String,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percent_used: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_percentage: Option<f64>,
    pub unit: String,
    pub period_end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct PlanSlot {
    #[serde(skip_serializing)]
    pub entitlement_id: Option<String>,
    #[serde(skip_serializing)]
    pub period_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_label: Option<String>,
    #[serde(skip_serializing)]
    pub pid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expire: Option<String>,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percent_used: Option<f64>,
    pub items: Vec<QuotaItem>,
}

#[derive(Debug, Clone, serde::Serialize, Default)]
pub struct QuotaOverview {
    #[serde(skip_serializing)]
    pub models: Vec<String>,
    pub account_id: String,
    pub revision: String,
    pub completion: String,
    pub errors: Vec<ScopeError>,
    pub total: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percent_used: Option<f64>,
    pub plan_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_expire: Option<String>,
    pub is_empty: bool,
    pub items: Vec<QuotaItem>,
    pub refreshed_at: i64,
    pub source: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<PlanSlot>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ScopeError {
    pub scope: String,
    pub code: QuotaError,
}

struct ScopedPart {
    account: crate::quota_auth::AccountScope,
    plan: crate::quota_auth::PlanScope,
    value: QuotaOverview,
}

fn merge_scoped(
    ctx: &AccountQueryContext,
    parts: Vec<ScopedPart>,
    errors: Vec<ScopeError>,
) -> QuotaOverview {
    if parts.iter().any(|part| part.account != ctx.scope) {
        return unavailable(
            &ctx.scope.snapshot_id,
            &ctx.scope.revision,
            QuotaError::ScopeChanged,
        );
    }
    let success = parts.len();
    let mut models = Vec::new();
    let mut groups: Vec<(crate::quota_auth::PlanScope, PlanSlot, i64)> = vec![];
    let mut refreshed = 0;
    for part in parts {
        for model in part.value.models {
            if !models.contains(&model) {
                models.push(model);
            }
        }
        refreshed = refreshed.max(part.value.refreshed_at);
        for mut slot in part.value.plans {
            let mut windows = Vec::new();
            merge_windows(&mut windows, std::mem::take(&mut slot.items), false);
            slot.items = windows;
            let duplicate = groups.iter().position(|(plan, old, _)| {
                plan == &part.plan
                    && slot.entitlement_id.is_some()
                    && slot.entitlement_id == old.entitlement_id
                    && slot.period_key == old.period_key
            });
            if let Some(index) = duplicate {
                let (_, old, time) = &mut groups[index];
                let newer = part.value.refreshed_at >= *time;
                let prefer = (slot_completeness(&slot), part.value.refreshed_at)
                    >= (slot_completeness(old), *time);
                let mut windows = old.items.clone();
                merge_windows(&mut windows, slot.items.clone(), newer);
                if prefer {
                    *old = slot;
                    *time = part.value.refreshed_at;
                }
                old.items = windows;
            } else {
                groups.push((part.plan.clone(), slot, part.value.refreshed_at));
            }
        }
    }
    let mut plans: Vec<_> = groups
        .into_iter()
        .map(|(_, mut slot, _)| {
            set_primary(&mut slot);
            slot
        })
        .collect();
    let primary = plans
        .iter()
        .max_by_key(|p| tier_rank(p.tier_code.as_deref()))
        .cloned()
        .unwrap_or_default();
    let items = plans.iter().flat_map(|p| p.items.iter().cloned()).collect();
    let complete = errors.is_empty() && success > 0;
    QuotaOverview {
        account_id: ctx.scope.snapshot_id.clone(),
        revision: ctx.scope.revision.clone(),
        completion: if complete {
            "complete"
        } else if success > 0 {
            "partial"
        } else {
            "unavailable"
        }
        .into(),
        is_empty: complete && plans.is_empty(),
        errors,
        refreshed_at: refreshed,
        total: primary.total,
        used: primary.used,
        remaining: primary.remaining,
        percent_used: primary.percent_used,
        plan_tier: primary.tier,
        plan_expire: primary.expire,
        items,
        plans: std::mem::take(&mut plans),
        source: "account".into(),
        models,
    }
}

fn item_completeness(item: &QuotaItem) -> usize {
    [item.total, item.used, item.remaining, item.percent_used]
        .iter()
        .filter(|v| v.is_some())
        .count()
}

fn slot_completeness(slot: &PlanSlot) -> usize {
    slot.items.iter().map(item_completeness).sum::<usize>() + usize::from(slot.expire.is_some())
}

// A complete window wins over an incomplete one; equally complete observations use
// the newer response. Repeated windows are replaced, never added together.
fn merge_windows(target: &mut Vec<QuotaItem>, incoming: Vec<QuotaItem>, newer: bool) {
    for item in incoming {
        if let Some(old) = target
            .iter_mut()
            .find(|old| item.window_key.is_some() && old.window_key == item.window_key)
        {
            if item_completeness(&item) > item_completeness(old)
                || (item_completeness(&item) == item_completeness(old) && newer)
            {
                *old = item;
            }
        } else {
            target.push(item);
        }
    }
}

fn set_primary(slot: &mut PlanSlot) {
    if let Some(item) = slot.items.iter().max_by_key(|i| item_completeness(i)) {
        slot.total = item.total;
        slot.used = item.used;
        slot.remaining = item
            .remaining
            .or_else(|| item.total.zip(item.used).map(|(t, u)| (t - u).max(0.0)));
        slot.percent_used = item.percent_used.or_else(|| {
            item.total
                .zip(item.used)
                .filter(|(t, _)| *t > 0.0)
                .map(|(t, u)| (u / t * 100.0).clamp(0.0, 100.0))
        });
    }
}

fn instance_id(value: &Value) -> Option<String> {
    ["user_plan_id", "subscription_id", "subscriptionId"]
        .iter()
        .find_map(|key| value.get(key).and_then(crate::quota_auth::normalized_id))
}

fn period_key(value: &Value) -> Option<String> {
    let start = [
        "starts_at",
        "period_start",
        "startTime",
        "subscribeStartTime",
    ]
    .iter()
    .find_map(|k| value.get(k));
    let end = [
        "ends_at",
        "period_end",
        "expires_at",
        "expireTime",
        "valid",
        "subscribeEndTime",
    ]
    .iter()
    .find_map(|k| value.get(k));
    (start.is_some() || end.is_some()).then(|| serde_json::json!([start, end]).to_string())
}

pub(crate) fn business_ok(v: &Value) -> bool {
    crate::quota_http::successful_data(v, false).is_ok()
}

pub(crate) fn is_active_coding_plan_entry(s: &Value) -> bool {
    ["productId", "productName"].iter().any(|k| {
        s.get(k)
            .and_then(Value::as_str)
            .is_some_and(|v| v.to_lowercase().contains("coding"))
    }) && s.get("status").and_then(Value::as_str) == Some("VALID")
        && s.get("inCurrentPeriod").and_then(Value::as_bool) == Some(true)
}

pub(crate) fn unavailable(id: &str, revision: &str, code: QuotaError) -> QuotaOverview {
    QuotaOverview {
        account_id: id.into(),
        revision: revision.into(),
        completion: "unavailable".into(),
        errors: vec![ScopeError {
            scope: "account".into(),
            code,
        }],
        ..Default::default()
    }
}

#[cfg(test)]
pub(crate) fn query_context_via(ctx: &AccountQueryContext, http: &dyn Transport) -> QuotaOverview {
    let service = TokenService::new(
        std::sync::Arc::new(|| chrono::Utc::now().timestamp_millis()),
        std::sync::Arc::new(|| 0),
    );
    service.activate(&ctx.scope);
    query_with_service(ctx, http, &service)
}

pub(crate) fn query_with_service(
    ctx: &AccountQueryContext,
    http: &dyn Transport,
    service: &TokenService,
) -> QuotaOverview {
    let mut parts = Vec::new();
    let mut errors = Vec::new();
    let start = (|| {
        let token = ctx.start_token()?;
        let raw = http.send(Request {
            method: "GET",
            url: format!("{BILLING_BALANCE_URL}?app_version={}", ctx.app_version),
            headers: zai_headers_with_version(
                ctx.app_version.clone(),
                token,
                ctx.device_mid.clone(),
            ),
            body: None,
        })?;
        validate_balance(&raw)?;
        Ok(normalize_balance(&raw))
    })();
    push_part(
        ctx,
        PlanScope::Start,
        "Start Plan",
        start,
        &mut parts,
        &mut errors,
    );
    match project_token::discover(ctx, http) {
        Err(code) => errors.push(ScopeError {
            scope: "Coding Plan".into(),
            code,
        }),
        Ok(discovery) => {
            if let Some(project) = discovery.personal {
                let scope = PlanScope::Personal {
                    organization_id: project.organization_id.clone(),
                    project_id: project.project_id.clone(),
                };
                query_project(
                    ctx,
                    &project,
                    scope,
                    None,
                    http,
                    service,
                    &mut parts,
                    &mut errors,
                );
            } else {
                errors.push(ScopeError {
                    scope: "Coding Plan".into(),
                    code: QuotaError::ProjectUnavailable,
                });
            }
            for project in discovery.teams {
                match project_token::team_subscription(ctx, &project, http) {
                    Err(code) => errors.push(ScopeError {
                        scope: project.label.clone(),
                        code,
                    }),
                    Ok(subscription) => {
                        let scope = PlanScope::Team {
                            organization_id: project.organization_id.clone(),
                            project_id: project.project_id.clone(),
                            product_id: subscription
                                .as_ref()
                                .and_then(|s| s.get("productId"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .into(),
                        };
                        if let Some(subscription) = subscription {
                            query_project(
                                ctx,
                                &project,
                                scope,
                                Some(subscription),
                                http,
                                service,
                                &mut parts,
                                &mut errors,
                            );
                        } else {
                            push_part(
                                ctx,
                                scope,
                                &project.label,
                                Ok(QuotaOverview::default()),
                                &mut parts,
                                &mut errors,
                            );
                        }
                    }
                }
            }
        }
    }
    let mut result = merge_scoped(ctx, parts, errors);
    result.refreshed_at = chrono::Utc::now().timestamp_millis();
    result
}

fn push_part(
    ctx: &AccountQueryContext,
    plan: PlanScope,
    label: &str,
    value: Result<QuotaOverview, QuotaError>,
    parts: &mut Vec<ScopedPart>,
    errors: &mut Vec<ScopeError>,
) {
    match value {
        Ok(mut value) => {
            value.refreshed_at = chrono::Utc::now().timestamp_millis();
            for slot in &mut value.plans {
                slot.scope_label = Some(label.into());
            }
            parts.push(ScopedPart {
                account: ctx.scope.clone(),
                plan,
                value,
            });
        }
        Err(code) => errors.push(ScopeError {
            scope: label.into(),
            code,
        }),
    }
}

fn query_project(
    ctx: &AccountQueryContext,
    project: &Project,
    scope: PlanScope,
    team_subscription: Option<Value>,
    http: &dyn Transport,
    service: &TokenService,
    parts: &mut Vec<ScopedPart>,
    errors: &mut Vec<ScopeError>,
) {
    let confirmed_team = team_subscription.as_ref().map(|detail| {
        subscription_slot(&serde_json::json!({
            "productId":detail.get("productId"),"productName":detail.get("productName"),
            "subscriptionId":detail.get("subscriptionId"),"valid":detail.get("subscribeEndTime"),
        }))
    });
    let result = (|| {
        let location = project_token::locate_key(ctx, project, http)?;
        let (subscriptions, malformed) = if let Some(detail) = team_subscription {
            (
                vec![serde_json::json!({
                    "productId":detail.get("productId"), "productName":detail.get("productName"),
                    "subscriptionId":detail.get("subscriptionId"), "valid":detail.get("subscribeEndTime"),
                    "status":"VALID", "inCurrentPeriod":true,
                })],
                false,
            )
        } else {
            let raw = service.fetch(ctx, &location, ProjectEndpoint::Subscriptions, http)?;
            personal_subscriptions(&raw)?
        };
        if malformed {
            errors.push(ScopeError {
                scope: project.label.clone(),
                code: QuotaError::InvalidResponse,
            });
        }
        if subscriptions.is_empty() {
            return Ok(QuotaOverview::default());
        }
        let raw = match service
            .fetch(ctx, &location, ProjectEndpoint::Quota, http)
            .and_then(|v| {
                validate_limits(&v)?;
                Ok(v)
            }) {
            Ok(raw) => raw,
            Err(code) => {
                errors.push(ScopeError {
                    scope: project.label.clone(),
                    code,
                });
                serde_json::json!({"code":0,"data":{"limits":[]}})
            }
        };
        let mut overview = normalize_quota_limit(
            &raw,
            Some(&serde_json::json!({"code":0,"data":subscriptions})),
        );
        if subscriptions.len() > 1 {
            // The monitor response is project-wide. Keep each subscription, but do
            // not copy the same project's usage into every entitlement instance.
            let usage = std::mem::take(&mut overview.plans);
            overview.plans = subscriptions.iter().map(subscription_slot).collect();
            if !overview.items.is_empty() {
                overview.plans.extend(usage.into_iter().map(|mut p| {
                    p.entitlement_id = None;
                    p.name = Some(project.label.clone());
                    p
                }));
            }
        } else if let Some(slot) = overview.plans.first_mut() {
            slot.entitlement_id = instance_id(&subscriptions[0]);
            slot.period_key = period_key(&subscriptions[0]);
        }
        Ok(overview)
    })();
    let result = match (result, confirmed_team) {
        (Err(code), Some(slot)) => {
            errors.push(ScopeError {
                scope: project.label.clone(),
                code,
            });
            Ok(QuotaOverview {
                plans: vec![slot],
                ..Default::default()
            })
        }
        (result, _) => result,
    };
    push_part(ctx, scope, &project.label, result, parts, errors);
}

fn subscription_slot(value: &Value) -> PlanSlot {
    let name = value
        .get("productName")
        .and_then(Value::as_str)
        .map(String::from);
    let tier = name.as_deref().map(tier_from_level);
    PlanSlot {
        entitlement_id: instance_id(value),
        period_key: period_key(value),
        name,
        pid: value
            .get("productId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .into(),
        tier_code: tier.as_deref().map(tier_code_from_display),
        tier,
        expire: extract_expire(value),
        ..Default::default()
    }
}

fn personal_subscriptions(raw: &Value) -> Result<(Vec<Value>, bool), QuotaError> {
    let list = crate::quota_http::successful_data(raw, false)?
        .as_array()
        .ok_or(QuotaError::InvalidResponse)?;
    let mut active = Vec::new();
    let mut malformed = false;
    for value in list {
        let coding = ["productId", "productName"].iter().any(|k| {
            value
                .get(k)
                .and_then(Value::as_str)
                .is_some_and(|s| s.to_lowercase().contains("coding"))
        });
        if !coding {
            if !value.is_object() {
                malformed = true;
            }
            continue;
        }
        let status = value.get("status").and_then(Value::as_str);
        if value
            .get("inCurrentPeriod")
            .and_then(Value::as_bool)
            .is_none()
            || !matches!(status, Some("VALID" | "EXPIRED" | "INVALID" | "CANCELLED"))
        {
            malformed = true;
            continue;
        }
        if is_active_coding_plan_entry(value) {
            active.push(value.clone());
        }
    }
    if active.is_empty() && malformed {
        Err(QuotaError::InvalidResponse)
    } else {
        Ok((active, malformed))
    }
}

fn validate_balance(raw: &Value) -> Result<(), QuotaError> {
    let data = crate::quota_http::successful_data(raw, false)?;
    let plans = data
        .get("plans")
        .and_then(Value::as_array)
        .ok_or(QuotaError::InvalidResponse)?;
    let balances = data
        .get("balances")
        .and_then(Value::as_array)
        .ok_or(QuotaError::InvalidResponse)?;
    if plans.iter().any(|p| {
        !p.is_object()
            || !p.get("status").and_then(Value::as_str).is_some_and(|s| {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "active" | "expired" | "cancelled" | "inactive"
                )
            })
    }) || balances.iter().any(|p| {
        !p.is_object()
            || ![
                "total_units",
                "used_units",
                "remaining_units",
                "available_units",
            ]
            .iter()
            .any(|k| p.get(k).and_then(to_number).is_some())
    }) {
        return Err(QuotaError::InvalidResponse);
    }
    Ok(())
}

fn validate_limits(raw: &Value) -> Result<(), QuotaError> {
    let data = crate::quota_http::successful_data(raw, false)?;
    let limits = data
        .get("limits")
        .and_then(Value::as_array)
        .ok_or(QuotaError::InvalidResponse)?;
    if limits.iter().any(|limit| {
        !limit.is_object()
            || !matches!(
                limit.get("type").and_then(Value::as_str),
                Some("TOKENS_LIMIT" | "TIME_LIMIT" | "CREDIT_LIMIT")
            )
    }) {
        return Err(QuotaError::InvalidResponse);
    }
    Ok(())
}

fn unit_label(unit: Option<i64>, number: Option<i64>) -> (String, String) {
    match unit {
        Some(3) => {
            let n = number.unwrap_or(5);
            (format!("每 {n} 小时"), format!("hours:{n}"))
        }
        Some(4) => ("每天".into(), "daily".into()),
        Some(5) => ("每月".into(), "monthly".into()),
        Some(6) => ("每周".into(), "weekly".into()),
        _ => ("每周期".into(), "cycle".into()),
    }
}

fn fmt_reset_time(ms: Option<i64>) -> Option<String> {
    let ms = ms?;
    if ms <= 0 {
        return None;
    }
    use chrono::TimeZone;
    Some(
        chrono::Local
            .timestamp_millis_opt(ms)
            .single()?
            .format("%m-%d %H:%M")
            .to_string(),
    )
}

fn safe_prefix(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut end = n;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn safe_suffix(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut start = s.len() - n;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

fn looks_like_date(d: &str) -> bool {
    d.len() == 10 && d.as_bytes().get(4) == Some(&b'-') && d.as_bytes().get(7) == Some(&b'-')
}

fn looks_like_dt(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 16 {
        return false;
    }
    let p = &b[..16];
    p[4] == b'-'
        && p[7] == b'-'
        && p[10] == b' '
        && p[13] == b':'
        && (0..16).all(|i| match i {
            0..=3 | 5..=6 | 8..=9 | 11..=12 | 14..=15 => p[i].is_ascii_digit(),
            _ => true,
        })
}

fn extract_expire(obj: &Value) -> Option<String> {
    const KEYS: [&str; 14] = [
        "ends_at",
        "subscribeEndTime",
        "nextRenewTime",
        "expireTime",
        "expire_time",
        "endTime",
        "end_time",
        "expireAt",
        "expiredTime",
        "validEndTime",
        "expires_at",
        "expiresAt",
        "expired_at",
        "period_end",
    ];
    for k in KEYS {
        let Some(v) = obj.get(k) else { continue };
        if let Some(n) = v.as_i64() {
            use chrono::TimeZone;
            if n > 1_000_000_000_000 {
                return chrono::Local
                    .timestamp_millis_opt(n)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string());
            }
            if n > 1_000_000_000 {
                return chrono::Local
                    .timestamp_opt(n, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M").to_string());
            }
        }
        if let Some(s) = v.as_str() {
            let t = s.trim();
            if t.is_empty() {
                continue;
            }
            if let Ok(n) = t.parse::<i64>() {
                use chrono::TimeZone;
                if n > 1_000_000_000_000 {
                    if let Some(dt) = chrono::Local.timestamp_millis_opt(n).single() {
                        return Some(dt.format("%Y-%m-%d %H:%M").to_string());
                    }
                } else if n > 1_000_000_000 {
                    if let Some(dt) = chrono::Local.timestamp_opt(n, 0).single() {
                        return Some(dt.format("%Y-%m-%d %H:%M").to_string());
                    }
                }
            }
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(t) {
                return Some(
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M")
                        .to_string(),
                );
            }
            let t = if t.as_bytes().get(10) == Some(&b'T') {
                format!("{} {}", &t[..10], &t[11..])
            } else {
                t.to_string()
            };
            let d = safe_prefix(&t, 10);
            if looks_like_date(d) {
                if looks_like_dt(&t) {
                    return Some(safe_prefix(&t, 16).to_string());
                }
                return Some(d.to_string());
            }
            return Some(t.to_string());
        }
    }
    if let Some(s) = obj.get("valid").and_then(|v| v.as_str()) {
        let tail = safe_suffix(s, 19);
        if looks_like_dt(tail) {
            return Some(safe_prefix(tail, 16).to_string());
        }
        if looks_like_date(safe_prefix(tail, 10)) {
            return Some(safe_prefix(tail, 10).to_string());
        }
        let d = safe_prefix(s, 10);
        if looks_like_date(d) {
            return Some(d.to_string());
        }
    }
    None
}

fn expiry_field(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(_) => {
            let mut o = serde_json::Map::new();
            o.insert("expires_at".to_string(), v.clone());
            extract_expire(&Value::Object(o))
        }
        _ => None,
    }
}

fn tier_from_level(level: &str) -> String {
    let l = level.to_lowercase();
    if l.contains("max") {
        "Max".into()
    } else if l.contains("pro") {
        "Pro".into()
    } else if l.contains("lite") {
        "Lite".into()
    } else {
        level.to_string()
    }
}

fn normalize_quota_limit(limit_resp: &Value, sub_resp: Option<&Value>) -> QuotaOverview {
    let data = limit_resp.get("data").cloned().unwrap_or(Value::Null);
    let limits = data
        .get("limits")
        .and_then(|l| l.as_array())
        .cloned()
        .unwrap_or_default();

    let mut items = vec![];
    let mut main: Option<QuotaItem> = None;
    for l in &limits {
        let typ = l.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let unit = l.get("unit").and_then(|u| u.as_i64());
        let number = l.get("number").and_then(|n| n.as_i64());
        let total = l.get("usage").and_then(|v| v.as_f64());
        let used = l.get("currentValue").and_then(|v| v.as_f64());
        let remaining = l.get("remaining").and_then(|v| v.as_f64());
        let percentage = l.get("percentage").and_then(|v| v.as_f64());
        let reset_ms = l.get("nextResetTime").and_then(|v| v.as_i64());
        let (period, window) = unit_label(unit, number);
        let (kind, name, unit_str, unit_code) = match typ {
            "TOKENS_LIMIT" => (
                "prompt_count",
                format!("提示次数（{period}）"),
                "次".to_string(),
                "count",
            ),
            "TIME_LIMIT" => (
                "duration",
                format!("使用时长（{period}）"),
                "分钟".to_string(),
                "minutes",
            ),
            _ => ("raw", format!("{typ}（{period}）"), String::new(), ""),
        };
        let percent_used = if let (Some(t), Some(u)) = (total, used) {
            if t > 0.0 {
                Some((u / t * 100.0).clamp(0.0, 100.0))
            } else {
                None
            }
        } else {
            percentage.map(|p| p.clamp(0.0, 100.0))
        };
        let reset = fmt_reset_time(reset_ms);
        let item = QuotaItem {
            window_key: Some(
                serde_json::json!([
                    typ,
                    unit,
                    number,
                    l.get("nextResetTime"),
                    l.get("startTime")
                ])
                .to_string(),
            ),
            name,
            total,
            used,
            remaining,
            percent_used,
            server_percentage: percentage,
            unit: unit_str,
            period_end: reset.as_ref().map(|r| format!("{r} 重置")),
            kind: (kind != "").then(|| kind.to_string()),
            window: Some(window),
            unit_code: (unit_code != "").then(|| unit_code.to_string()),
            reset,
        };
        if typ == "TIME_LIMIT" && total.is_some() {
            main = main.or(Some(item.clone()));
        }
        items.push(item);
    }

    let level = data.get("level").and_then(|l| l.as_str()).map(String::from);
    let mut plan_tier = level.as_deref().map(tier_from_level);
    let mut plan_expire: Option<String> = None;
    let mut product_name: Option<String> = None;
    if let Some(sub) = sub_resp {
        if business_ok(sub) {
            if let Some(arr) = sub.get("data").and_then(|d| d.as_array()) {
                let current = arr
                    .iter()
                    .find(|s| is_active_coding_plan_entry(s))
                    .or_else(|| arr.first());
                if let Some(s) = current {
                    if let Some(pn) = s.get("productName").and_then(|x| x.as_str()) {
                        if !pn.trim().is_empty() {
                            product_name = Some(pn.to_string());
                            plan_tier = Some(tier_from_level(pn));
                        }
                    }
                    plan_expire = extract_expire(s);
                }
            }
        }
    }

    let main = main.or_else(|| items.iter().find(|i| i.total.is_some()).cloned());
    let (total, used, remaining, percent_used) = match &main {
        Some(m) => (m.total, m.used, m.remaining, m.percent_used),
        None => {
            let p = items.iter().find_map(|i| i.percent_used);
            (None, None, None, p)
        }
    };

    let plans = if plan_tier.is_some() || !items.is_empty() {
        vec![PlanSlot {
            pid: String::new(),
            tier: plan_tier.clone(),
            tier_code: plan_tier.as_deref().map(tier_code_from_display),
            name: product_name,
            expire: plan_expire.clone(),
            total,
            used,
            remaining,
            percent_used,
            items: items.clone(),
            ..Default::default()
        }]
    } else {
        vec![]
    };

    QuotaOverview {
        total,
        used,
        remaining,
        percent_used,
        plan_tier,
        plan_expire,
        is_empty: limits.is_empty(),
        items,
        refreshed_at: 0,
        source: String::new(),
        plans,
        ..Default::default()
    }
}

fn unwrap(data: &Value) -> Value {
    let mut cur = data.clone();
    for _ in 0..4 {
        if !cur.is_object() {
            return cur;
        }
        if let Some(d) = cur.get("data") {
            cur = d.clone();
            continue;
        }
        if let Some(r) = cur.get("result") {
            cur = r.clone();
            continue;
        }
        break;
    }
    cur
}

fn to_number(v: &Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return if n.is_finite() { Some(n) } else { None };
    }
    if let Some(s) = v.as_str() {
        let t = s.replace(',', "");
        return t.trim().parse::<f64>().ok();
    }
    None
}

fn plan_tier_from_id(plan_id: &str, name: Option<&str>) -> (String, String) {
    let mut hay = plan_id.to_lowercase();
    if let Some(n) = name {
        hay.push(' ');
        hay.push_str(&n.to_lowercase());
    }
    if hay.contains("max") {
        ("Max".into(), "max".into())
    } else if hay.contains("pro") {
        ("Pro".into(), "pro".into())
    } else if hay.contains("lite") {
        ("Lite".into(), "lite".into())
    } else if hay.contains("start") {
        ("Start Plan".into(), "start".into())
    } else if [
        "trial",
        "taste",
        "experience",
        "gift",
        "weekend",
        "promo",
        "activity",
        "体验",
    ]
    .iter()
    .any(|k| hay.contains(k))
    {
        ("体验".into(), "trial".into())
    } else {
        (plan_id.to_string(), "other".into())
    }
}

fn tier_code_from_display(tier: &str) -> String {
    let t = tier.to_lowercase();
    if t.contains("max") {
        "max".into()
    } else if t.contains("pro") {
        "pro".into()
    } else if t.contains("lite") {
        "lite".into()
    } else if t.contains("start") {
        "start".into()
    } else if t.contains("trial") || tier.contains("体验") {
        "trial".into()
    } else {
        "other".into()
    }
}

fn tier_rank(code: Option<&str>) -> u8 {
    match code.unwrap_or("") {
        "max" => 5,
        "pro" => 4,
        "lite" => 3,
        "start" => 2,
        "trial" => 1,
        _ => 0,
    }
}

fn normalize_balance(balance_data: &Value) -> QuotaOverview {
    let mut balance = unwrap(balance_data);
    let now = balance
        .get("server_time")
        .and_then(to_number)
        .filter(|n| n.is_finite() && *n >= 0.0)
        .unwrap_or_else(|| chrono::Utc::now().timestamp() as f64);
    let all_plans = balance
        .get("plans")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let expired = |plan: &Value| {
        plan.get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("expired"))
            || plan
                .get("ends_at")
                .and_then(to_number)
                .is_some_and(|end| end > 0.0 && end <= now)
    };
    // Filter buckets using their user-plan instance. A product ID is only a fallback
    // when it cannot select a different instance of the same product.
    if let Some(balances) = balance.get_mut("balances").and_then(Value::as_array_mut) {
        balances.retain(|bucket| {
            let owners: Vec<_> = all_plans
                .iter()
                .filter(|plan| {
                    let instance = instance_id(bucket);
                    if instance.is_some() && instance_id(plan).is_some() {
                        instance == instance_id(plan)
                    } else {
                        bpid_for(bucket).is_some() && bpid_for(bucket) == bpid_for(plan)
                    }
                })
                .collect();
            owners.is_empty() || owners.iter().any(|plan| !expired(plan))
        });
    }
    let mut slots: Vec<PlanSlot> = all_plans
        .iter()
        .filter(|plan| {
            !expired(plan)
                && plan
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.trim().eq_ignore_ascii_case("active"))
        })
        .map(|plan| {
            let pid = bpid_for(plan).unwrap_or("").to_string();
            let name = plan
                .get("name")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(String::from);
            let (tier, tier_code) = plan_tier_from_id(&pid, name.as_deref());
            PlanSlot {
                entitlement_id: instance_id(plan),
                period_key: period_key(plan),
                name: Some(name.unwrap_or_else(|| pid.clone())),
                pid,
                tier: Some(tier),
                tier_code: Some(tier_code),
                expire: extract_expire(plan),
                ..Default::default()
            }
        })
        .collect();
    let mut loose = Vec::new();
    if let Some(balances) = balance.get("balances").and_then(Value::as_array) {
        for bucket in balances {
            let total = bucket.get("total_units").and_then(to_number);
            let used = bucket.get("used_units").and_then(to_number);
            let remaining = bucket.get("remaining_units").and_then(to_number);
            let window_key = bucket
                .get("bucket_id")
                .and_then(Value::as_str)
                .map(|id| format!("bucket:{id}"))
                .or_else(|| {
                    let entitlement = bucket.get("entitlement_id").and_then(Value::as_str)?;
                    let period = period_key(bucket)?;
                    Some(
                        serde_json::json!([
                            instance_id(bucket),
                            entitlement,
                            period,
                            bucket.get("meter"),
                            bucket.get("unit_type")
                        ])
                        .to_string(),
                    )
                });
            let item = QuotaItem {
                window_key,
                name: ["show_name", "name", "entitlement_id"]
                    .iter()
                    .find_map(|k| bucket.get(k).and_then(Value::as_str))
                    .unwrap_or("Unknown")
                    .into(),
                total,
                used,
                remaining,
                percent_used: total
                    .zip(used)
                    .filter(|(t, _)| *t > 0.0)
                    .map(|(t, u)| (u / t * 100.0).clamp(0.0, 100.0)),
                unit: bucket
                    .get("unit_type")
                    .or_else(|| bucket.get("meter"))
                    .and_then(Value::as_str)
                    .unwrap_or("quota")
                    .into(),
                period_end: bucket
                    .get("expires_at")
                    .or_else(|| bucket.get("period_end"))
                    .and_then(expiry_field),
                ..Default::default()
            };
            let instance = instance_id(bucket);
            let matches: Vec<usize> = slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| {
                    if instance.is_some() {
                        slot.entitlement_id == instance
                    } else {
                        bpid_for(bucket).is_some_and(|pid| slot.pid == pid)
                    }
                })
                .map(|(index, _)| index)
                .collect();
            if matches.len() == 1 {
                slots[matches[0]].items.push(item);
            } else {
                loose.push(item);
            }
        }
    }
    if !loose.is_empty() {
        slots.push(PlanSlot {
            name: Some("Other quota".into()),
            tier_code: Some("other".into()),
            items: loose,
            ..Default::default()
        });
    }
    for slot in &mut slots {
        let mut items = Vec::new();
        merge_windows(&mut items, std::mem::take(&mut slot.items), false);
        slot.items = items;
        set_primary(slot);
    }
    let primary = slots
        .iter()
        .max_by_key(|slot| tier_rank(slot.tier_code.as_deref()))
        .cloned()
        .unwrap_or_default();
    QuotaOverview {
        total: primary.total,
        used: primary.used,
        remaining: primary.remaining,
        percent_used: primary.percent_used,
        plan_tier: primary.tier,
        plan_expire: primary.expire,
        is_empty: slots.is_empty(),
        items: slots.iter().flat_map(|p| p.items.iter().cloned()).collect(),
        plans: slots,
        models: balance_models(&balance),
        ..Default::default()
    }
}

fn bpid_for(item: &Value) -> Option<&str> {
    item.get("plan_id")
        .or_else(|| item.get("planId"))
        .and_then(Value::as_str)
}

fn balance_models(balance: &Value) -> Vec<String> {
    let mut models = Vec::new();
    if let Some(balances) = balance.get("balances").and_then(Value::as_array) {
        for item in balances {
            let mut found: Vec<String> = item
                .get("capabilities")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|s| s.strip_prefix("model:"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(canonical_model_id)
                .collect();
            if found.is_empty() {
                if let Some(name) = item
                    .get("show_name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                {
                    found.push(canonical_model_id(name.trim()));
                }
            }
            for name in found {
                if !models.contains(&name) {
                    models.push(name);
                }
            }
        }
    }
    models
}
