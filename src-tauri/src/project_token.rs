//! Project discovery and ephemeral credentials for one immutable account login.
use crate::quota_auth::AccountScope;
use crate::quota_auth::{AccountQueryContext, ProviderFamily, QuotaError};
use crate::quota_http::{successful_data, Request, Transport};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, OnceLock};

pub(crate) struct TokenService {
    state: Mutex<ServiceState>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    jitter: Arc<dyn Fn() -> i64 + Send + Sync>,
}
#[derive(Default)]
struct ServiceState {
    revisions: HashMap<String, String>,
    entries: HashMap<CacheKey, Arc<Entry>>,
}
#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    account: AccountScope,
    business_subject: Option<String>,
    origin: &'static str,
    organization_id: String,
    project_id: String,
    api_key_id: String,
    usage_scene: u8,
}
#[derive(Default)]
struct Entry {
    state: Mutex<EntryState>,
    changed: Condvar,
}
#[derive(Default)]
struct EntryState {
    value: Option<TokenMaterial>,
    refresh_at: i64,
    pending: bool,
    result: Option<Result<TokenMaterial, QuotaError>>,
}
#[derive(Clone)]
pub(crate) struct TokenMaterial {
    pub token: String,
    pub expires_at: i64,
}
#[derive(Clone, Copy)]
pub(crate) enum ProjectEndpoint {
    Subscriptions,
    Quota,
}

impl TokenService {
    pub(crate) fn new(
        clock: Arc<dyn Fn() -> i64 + Send + Sync>,
        jitter: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            state: Mutex::new(ServiceState::default()),
            clock,
            jitter,
        }
    }
    pub(crate) fn activate(&self, scope: &AccountScope) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.revisions.get(&scope.snapshot_id) != Some(&scope.revision) {
            state
                .entries
                .retain(|key, _| key.account.snapshot_id != scope.snapshot_id);
            state
                .revisions
                .insert(scope.snapshot_id.clone(), scope.revision.clone());
        }
    }
    pub(crate) fn invalidate(&self, id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.retain(|key, _| key.account.snapshot_id != id);
        state.revisions.remove(id);
    }
    pub(crate) fn invalidate_if_current(&self, scope: &AccountScope) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.revisions.get(&scope.snapshot_id) == Some(&scope.revision) {
            state
                .entries
                .retain(|key, _| key.account.snapshot_id != scope.snapshot_id);
            state.revisions.remove(&scope.snapshot_id);
        }
    }
    pub(crate) fn retain_revision(&self, id: &str, revision: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.revisions.get(id).is_some_and(|old| old != revision) {
            state.entries.retain(|key, _| key.account.snapshot_id != id);
            state.revisions.remove(id);
        }
    }
    pub(crate) fn reconcile(&self, revisions: &[(String, String)]) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let keep = |id: &str, revision: &str| {
            id == "live"
                || revisions.iter().any(|(current_id, current_revision)| {
                    current_id == id && current_revision == revision
                })
        };
        state
            .entries
            .retain(|key, _| keep(&key.account.snapshot_id, &key.account.revision));
        state.revisions.retain(|id, revision| keep(id, revision));
    }
    pub(crate) fn assert_current(&self, scope: &AccountScope) -> Result<(), QuotaError> {
        if self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .revisions
            .get(&scope.snapshot_id)
            == Some(&scope.revision)
        {
            Ok(())
        } else {
            Err(QuotaError::ScopeChanged)
        }
    }
    fn key(ctx: &AccountQueryContext, location: &KeyLocation) -> CacheKey {
        CacheKey {
            account: ctx.scope.clone(),
            origin: ctx.scope.family.origin(),
            business_subject: ctx.auth.business_subject.clone(),
            organization_id: location.project.organization_id.clone(),
            project_id: location.project.project_id.clone(),
            api_key_id: location.api_key_id.clone(),
            usage_scene: 1,
        }
    }
    fn reject(&self, ctx: &AccountQueryContext, location: &KeyLocation, token: &str) {
        let entry = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .get(&Self::key(ctx, location))
            .cloned();
        if let Some(entry) = entry {
            let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .value
                .as_ref()
                .is_some_and(|value| value.token == token)
            {
                state.value = None;
            }
        }
    }
    pub(crate) fn resolve(
        &self,
        ctx: &AccountQueryContext,
        location: &KeyLocation,
        http: &dyn Transport,
        rejected: Option<&str>,
    ) -> Result<TokenMaterial, QuotaError> {
        ctx.business_token()?;
        if let Some(token) = rejected {
            self.reject(ctx, location, token);
        }
        let entry = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.revisions.get(&ctx.scope.snapshot_id) != Some(&ctx.scope.revision) {
                return Err(QuotaError::ScopeChanged);
            }
            state
                .entries
                .entry(Self::key(ctx, location))
                .or_default()
                .clone()
        };
        let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.pending {
            while state.pending {
                state = entry.changed.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            self.assert_current(&ctx.scope)?;
            return state
                .result
                .clone()
                .unwrap_or(Err(QuotaError::InvalidResponse));
        }
        if let Some(value) = &state.value {
            if (self.clock)() < state.refresh_at.min(value.expires_at) {
                self.assert_current(&ctx.scope)?;
                return Ok(value.clone());
            }
        }
        state.pending = true;
        drop(state);
        let started = (self.clock)();
        let issued = (|| {
            self.assert_current(&ctx.scope)?;
            let raw = http.send(Request {
                method: "POST",
                url: format!(
                    "{}/{}/access_tokens",
                    key_list_url(ctx, &location.project),
                    encode_segment(&location.api_key_id)
                ),
                headers: vec![
                    (
                        "Authorization".into(),
                        format!("Bearer {}", ctx.business_token()?),
                    ),
                    ("Content-Type".into(), "application/json".into()),
                ],
                body: Some(
                    serde_json::json!({"clientType":"zcode", "clientVersion":ctx.app_version}),
                ),
            })?;
            let data = successful_data(&raw, true)?;
            match data.get("enable") {
                Some(Value::Bool(false)) => return Err(QuotaError::TokenDisabled),
                None | Some(Value::Bool(true)) => {}
                _ => return Err(QuotaError::InvalidResponse),
            }
            if data.get("tokenType").and_then(Value::as_str) != Some("Bearer") {
                return Err(QuotaError::InvalidResponse);
            }
            let duration = seconds_ms(data.get("expiresIn"))?;
            let absolute = seconds_ms(data.get("expiresAt"))?;
            let expires_at = absolute.min(started.saturating_add(duration));
            if expires_at <= (self.clock)() {
                return Err(QuotaError::InvalidResponse);
            }
            Ok(TokenMaterial {
                token: required_string(data, "accessToken")?,
                expires_at,
            })
        })();
        let mut state = entry.state.lock().unwrap_or_else(|e| e.into_inner());
        let result = if self.assert_current(&ctx.scope).is_err() {
            state.value = None;
            Err(QuotaError::ScopeChanged)
        } else {
            match issued {
                Ok(value) => {
                    state.refresh_at = value
                        .expires_at
                        .saturating_sub(120_000 + (self.jitter)().clamp(0, 60_000));
                    state.value = Some(value.clone());
                    Ok(value)
                }
                Err(error)
                    if matches!(error, QuotaError::NetworkError | QuotaError::RateLimited)
                        && state
                            .value
                            .as_ref()
                            .is_some_and(|v| (self.clock)() < v.expires_at) =>
                {
                    let value = state.value.as_ref().unwrap().clone();
                    state.refresh_at = ((self.clock)() + 5_000).min(value.expires_at);
                    Ok(value)
                }
                Err(error) => {
                    state.value = None;
                    Err(error)
                }
            }
        };
        state.pending = false;
        state.result = Some(result.clone());
        entry.changed.notify_all();
        result
    }
    pub(crate) fn fetch(
        &self,
        ctx: &AccountQueryContext,
        location: &KeyLocation,
        endpoint: ProjectEndpoint,
        http: &dyn Transport,
    ) -> Result<Value, QuotaError> {
        let mut rejected = None;
        for attempt in 0..2 {
            let material = self.resolve(ctx, location, http, rejected.as_deref())?;
            self.assert_current(&ctx.scope)?;
            let path = match endpoint {
                ProjectEndpoint::Subscriptions if !location.project.team => {
                    "/api/biz/subscription/list"
                }
                ProjectEndpoint::Subscriptions => return Err(QuotaError::ProjectUnavailable),
                ProjectEndpoint::Quota if location.project.team => {
                    "/api/monitor/usage/quota/limit?type=2"
                }
                ProjectEndpoint::Quota => "/api/monitor/usage/quota/limit",
            };
            // Both monitor and personal subscription endpoints take the raw project token.
            let mut headers = vec![("Authorization".into(), material.token.clone())];
            if location.project.team {
                headers.push((
                    "bigmodel-organization".into(),
                    location.project.organization_id.clone(),
                ));
                headers.push((
                    "bigmodel-project".into(),
                    location.project.project_id.clone(),
                ));
            }
            let result = http
                .send(Request {
                    method: "GET",
                    url: format!("{}{path}", ctx.scope.family.origin()),
                    headers,
                    body: None,
                })
                .and_then(|raw| {
                    successful_data(&raw, false)?;
                    Ok(raw)
                });
            self.assert_current(&ctx.scope)?;
            if result == Err(QuotaError::LoginRequired) {
                self.reject(ctx, location, &material.token);
                if attempt == 0 {
                    rejected = Some(material.token);
                    continue;
                }
            }
            return result;
        }
        Err(QuotaError::LoginRequired)
    }
}

fn seconds_ms(value: Option<&Value>) -> Result<i64, QuotaError> {
    let value = value
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v > 0.0 && *v < (i64::MAX / 1000) as f64)
        .ok_or(QuotaError::InvalidResponse)?;
    Ok((value * 1000.0) as i64)
}

pub(crate) fn service() -> &'static TokenService {
    static SERVICE: OnceLock<TokenService> = OnceLock::new();
    SERVICE.get_or_init(TokenService::default)
}

impl Default for TokenService {
    fn default() -> Self {
        Self::new(
            Arc::new(|| chrono::Utc::now().timestamp_millis()),
            Arc::new(|| (uuid::Uuid::new_v4().as_u128() % 60_001) as i64),
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Project {
    pub organization_id: String,
    pub project_id: String,
    pub label: String,
    pub team: bool,
}

pub(crate) struct Discovery {
    pub personal: Option<Project>,
    pub teams: Vec<Project>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct KeyLocation {
    pub project: Project,
    pub api_key_id: String,
}

pub(crate) fn discover(
    ctx: &AccountQueryContext,
    http: &dyn Transport,
) -> Result<Discovery, QuotaError> {
    let raw = http.send(Request {
        method: "GET",
        url: format!(
            "{}/api/biz/customer/getCustomerInfo",
            ctx.scope.family.origin()
        ),
        headers: management_headers(ctx, None, false)?,
        body: None,
    })?;
    let data = successful_data(&raw, true)?;
    let organizations = data
        .get("organizations")
        .and_then(Value::as_array)
        .ok_or(QuotaError::InvalidResponse)?;
    let mut personal = Vec::new();
    let mut teams = Vec::new();
    for org in organizations {
        let org_id = required_string(org, "organizationId")?;
        let org_name = org
            .get("organizationName")
            .and_then(Value::as_str)
            .unwrap_or("");
        let projects = org
            .get("projects")
            .and_then(Value::as_array)
            .ok_or(QuotaError::InvalidResponse)?;
        for value in projects {
            let project_id = required_string(value, "projectId")?;
            let project_name = value
                .get("projectName")
                .and_then(Value::as_str)
                .unwrap_or("");
            let typ = value
                .get("projectType")
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                        .ok_or(QuotaError::InvalidResponse)
                })
                .transpose()?;
            if typ.is_some_and(|t| ![0, 1, 2].contains(&t)) {
                return Err(QuotaError::InvalidResponse);
            }
            let project = Project {
                organization_id: org_id.clone(),
                project_id,
                label: if project_name.is_empty() {
                    "Coding Plan".into()
                } else if org_name.is_empty() {
                    project_name.into()
                } else {
                    format!("{org_name} / {project_name}")
                },
                team: typ == Some(2),
            };
            if project.team {
                if !teams.iter().any(|p: &Project| {
                    p.organization_id == project.organization_id
                        && p.project_id == project.project_id
                }) {
                    teams.push(project);
                }
            } else {
                personal.push((
                    !org_name.contains("默认机构"),
                    !project_name.contains("默认项目"),
                    project,
                ));
            }
        }
    }
    // Stable ordering keeps the official first-organization / default-project fallback.
    // Pick the organization first, then its default project (never a team project).
    personal.sort_by_key(|p| p.0);
    let first_org = personal.first().map(|p| p.2.organization_id.clone());
    personal.retain(|p| Some(&p.2.organization_id) == first_org.as_ref());
    personal.sort_by_key(|p| p.1);
    Ok(Discovery {
        personal: personal.into_iter().next().map(|p| p.2),
        teams,
    })
}

pub(crate) fn locate_key(
    ctx: &AccountQueryContext,
    project: &Project,
    http: &dyn Transport,
) -> Result<KeyLocation, QuotaError> {
    let raw = http.send(Request {
        method: "GET",
        url: key_list_url(ctx, project),
        headers: management_headers(ctx, Some(project), project.team)?,
        body: None,
    })?;
    let list = successful_data(&raw, true)?
        .as_array()
        .ok_or(QuotaError::InvalidResponse)?;
    let name = if project.team {
        "zcode-team-api-key"
    } else {
        "zcode-api-key"
    };
    // Match the official existing-key contract. usageScene is a creation field;
    // existing keys can return legacy sentinels such as -1. It must not decide
    // whether an account has Coding Plan or whether its scoped key can be reused.
    let key = list
        .iter()
        .find(|key| {
            key.get("name").and_then(Value::as_str) == Some(name)
                && (!project.team
                    || (key.get("keyType").and_then(Value::as_i64) == Some(2)
                        && required_string(key, "apiKey").is_ok()))
        })
        .ok_or(QuotaError::ProjectKeyMissing)?;
    if !project.team && key.get("keyType").and_then(Value::as_i64) == Some(2) {
        return Err(QuotaError::ProjectKeyMissing);
    }
    let api_key_id = required_string(key, "apiKey").map_err(|_| QuotaError::ProjectKeyMissing)?;
    Ok(KeyLocation {
        project: project.clone(),
        api_key_id,
    })
}

pub(crate) fn team_subscription(
    ctx: &AccountQueryContext,
    project: &Project,
    http: &dyn Transport,
) -> Result<Option<Value>, QuotaError> {
    if !project.team {
        return Err(QuotaError::ProjectUnavailable);
    }
    let raw = http.send(Request {
        method: "GET",
        url: format!(
            "{}/api/biz/team/subscribe/product/querySubscribeDetail",
            ctx.scope.family.origin()
        ),
        headers: management_headers(ctx, Some(project), false)?,
        body: None,
    })?;
    let data = successful_data(&raw, false)?;
    match data.get("hasSubscription").and_then(Value::as_bool) {
        Some(false) => Ok(None),
        Some(true) => match (
            data.get("status").and_then(Value::as_str),
            data.get("memberGrantStatus").and_then(Value::as_str),
        ) {
            (Some("EXPIRED"), _) | (Some("EFFECTIVE"), Some("UNASSIGNED")) => Ok(None),
            (Some("EFFECTIVE"), Some("VALID")) => {
                required_string(data, "productId")?;
                Ok(Some(data.clone()))
            }
            _ => Err(QuotaError::InvalidResponse),
        },
        None => Err(QuotaError::InvalidResponse),
    }
}

fn required_string(value: &Value, key: &str) -> Result<String, QuotaError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .ok_or(QuotaError::InvalidResponse)
}

fn management_headers(
    ctx: &AccountQueryContext,
    project: Option<&Project>,
    raw_team_key_auth: bool,
) -> Result<Vec<(String, String)>, QuotaError> {
    let token = ctx.business_token()?;
    let auth = if ctx.scope.family == ProviderFamily::Zai && !raw_team_key_auth {
        format!("Bearer {token}")
    } else {
        token.into()
    };
    let mut headers = vec![
        ("Authorization".into(), auth),
        ("Content-Type".into(), "application/json".into()),
    ];
    if let Some(project) = project.filter(|p| p.team) {
        headers.push((
            "bigmodel-organization".into(),
            project.organization_id.clone(),
        ));
        headers.push(("bigmodel-project".into(), project.project_id.clone()));
    }
    Ok(headers)
}

fn encode_segment(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn key_list_url(ctx: &AccountQueryContext, project: &Project) -> String {
    format!(
        "{}/api/biz/v1/organization/{}/projects/{}/api_keys",
        ctx.scope.family.origin(),
        encode_segment(&project.organization_id),
        encode_segment(&project.project_id)
    )
}

#[cfg(test)]
mod tests;
