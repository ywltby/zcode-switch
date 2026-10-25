//! Account-scoped login material. These types deliberately do not serialize secrets.
use crate::store::Account;
use crate::zcrypto;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderFamily {
    Zai,
    Bigmodel,
}

impl ProviderFamily {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Zai => "zai",
            Self::Bigmodel => "bigmodel",
        }
    }
    pub(crate) fn origin(self) -> &'static str {
        match self {
            Self::Zai => "https://api.z.ai",
            Self::Bigmodel => "https://bigmodel.cn",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AccountScope {
    pub snapshot_id: String,
    pub family: ProviderFamily,
    pub zcode_subject: String,
    pub revision: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PlanScope {
    Start,
    Personal {
        organization_id: String,
        project_id: String,
    },
    Team {
        organization_id: String,
        project_id: String,
        product_id: String,
    },
}

pub(crate) struct AccountAuth {
    pub zcode_jwt: Option<String>,
    pub business_token: Option<String>,
    pub business_error: Option<QuotaError>,
    pub business_subject: Option<String>,
}

pub(crate) struct AccountQueryContext {
    pub scope: AccountScope,
    pub auth: AccountAuth,
    pub device_mid: Option<String>,
    pub app_version: String,
}

impl AccountQueryContext {
    pub(crate) fn start_token(&self) -> Result<&str, QuotaError> {
        let token = valid_token(self.auth.zcode_jwt.as_deref())?;
        if self.device_mid.is_none() {
            return Err(QuotaError::DeviceIdentityMissing);
        }
        Ok(token)
    }
    pub(crate) fn business_token(&self) -> Result<&str, QuotaError> {
        if let Some(error) = self.auth.business_error {
            return Err(error);
        }
        let token = valid_token(self.auth.business_token.as_deref())?;
        if self.auth.zcode_jwt.as_deref() == Some(token) {
            return Err(QuotaError::LoginRequired);
        }
        Ok(token)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuotaError {
    IdentityMissing,
    IdentityConflict,
    LoginRequired,
    DeviceIdentityMissing,
    ProjectUnavailable,
    ProjectKeyMissing,
    TokenDisabled,
    RateLimited,
    NetworkError,
    InvalidResponse,
    ScopeChanged,
}

impl QuotaError {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::IdentityMissing => "identity_missing",
            Self::IdentityConflict => "identity_conflict",
            Self::LoginRequired => "login_required",
            Self::DeviceIdentityMissing => "device_identity_missing",
            Self::ProjectUnavailable => "project_unavailable",
            Self::ProjectKeyMissing => "project_key_missing",
            Self::TokenDisabled => "token_disabled",
            Self::RateLimited => "rate_limited",
            Self::NetworkError => "network_error",
            Self::InvalidResponse => "invalid_response",
            Self::ScopeChanged => "scope_changed",
        }
    }
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

fn valid_token(token: Option<&str>) -> Result<&str, QuotaError> {
    let token = token
        .filter(|s| !s.trim().is_empty())
        .ok_or(QuotaError::LoginRequired)?;
    if zcrypto::jwt_exp_ms(token).is_some_and(|exp| exp <= chrono::Utc::now().timestamp_millis()) {
        return Err(QuotaError::LoginRequired);
    }
    Ok(token)
}

fn read_secret(creds: &Value, key: &str, secret: &str) -> Result<Option<String>, QuotaError> {
    let Some(value) = creds.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let text = value.as_str().ok_or(QuotaError::InvalidResponse)?;
    let plain = if zcrypto::is_encrypted(text) {
        zcrypto::decrypt_with_secret(text, secret).map_err(|_| QuotaError::LoginRequired)?
    } else {
        text.to_string()
    };
    Ok((!plain.trim().is_empty()).then(|| plain.trim().to_string()))
}

pub(crate) fn normalized_id(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.to_string()),
        _ => None,
    }
}

fn subject(value: &Value, fields: &[&str]) -> Result<Option<String>, QuotaError> {
    let mut found: Option<String> = None;
    for field in fields {
        if let Some(id) = value.get(field).and_then(normalized_id) {
            if found.as_ref().is_some_and(|old| old != &id) {
                return Err(QuotaError::IdentityConflict);
            }
            found = Some(id);
        }
    }
    Ok(found)
}

fn family(creds: &Value, secret: &str) -> Result<ProviderFamily, QuotaError> {
    match read_secret(creds, "oauth:active_provider", secret)?.as_deref() {
        Some("zai") => Ok(ProviderFamily::Zai),
        Some("bigmodel") => Ok(ProviderFamily::Bigmodel),
        Some(_) => Err(QuotaError::IdentityMissing),
        None => {
            let mut choices = Vec::new();
            for f in [ProviderFamily::Zai, ProviderFamily::Bigmodel] {
                let prefix = format!("oauth:{}", f.as_str());
                if read_secret(creds, &format!("{prefix}:access_token"), secret)?.is_some()
                    && read_secret(creds, &format!("{prefix}:user_info"), secret)?.is_some()
                {
                    choices.push(f);
                }
            }
            if choices.len() == 1 {
                Ok(choices[0])
            } else {
                Err(QuotaError::IdentityMissing)
            }
        }
    }
}

/// Also works for incomplete/expired logins, so the UI can invalidate old results.
/// Names and unrelated historical provider credentials never affect this digest.
pub(crate) fn quota_revision(snapshot: &Account, secret: &str) -> String {
    let mut material = vec![json!(snapshot.id), json!(snapshot.virtual_device_mid)];
    let f = family(&snapshot.credentials, secret).ok();
    for key in ["oauth:active_provider", "zcodejwttoken"]
        .into_iter()
        .map(String::from)
        .chain(
            [ProviderFamily::Zai, ProviderFamily::Bigmodel]
                .into_iter()
                .filter(|candidate| f.is_none() || Some(*candidate) == f)
                .flat_map(|candidate| {
                    [
                        format!("oauth:{}:access_token", candidate.as_str()),
                        format!("oauth:{}:user_info", candidate.as_str()),
                    ]
                }),
        )
    {
        let raw = snapshot
            .credentials
            .get(&key)
            .cloned()
            .unwrap_or(Value::Null);
        let value = read_secret(&snapshot.credentials, &key, secret)
            .ok()
            .flatten()
            .map(Value::String)
            .unwrap_or(raw);
        // Profile display names are not authentication material.
        let value = if key.ends_with(":user_info") {
            value
                .as_str()
                .and_then(|s| serde_json::from_str::<Value>(s).ok())
                .map(|v| json!([v.get("id"), v.get("user_id")]))
                .unwrap_or(value)
        } else {
            value
        };
        material.push(json!([key, value]));
    }
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&material).expect("JSON values"))
    )
}

pub(crate) fn resolve_account_context(
    snapshot: &Account,
    secret: &str,
    app_version: &str,
) -> Result<AccountQueryContext, QuotaError> {
    let creds = &snapshot.credentials;
    let family = family(creds, secret)?;
    let profile = read_secret(
        creds,
        &format!("oauth:{}:user_info", family.as_str()),
        secret,
    )?
    .map(|s| serde_json::from_str::<Value>(&s).map_err(|_| QuotaError::InvalidResponse))
    .transpose()?;
    let zcode_jwt = read_secret(creds, "zcodejwttoken", secret)?;
    let jwt_payload = zcode_jwt
        .as_deref()
        .map(|s| zcrypto::decode_jwt(s).ok_or(QuotaError::LoginRequired))
        .transpose()?;
    let profile_id = profile
        .as_ref()
        .map(|v| subject(v, &["id", "user_id"]))
        .transpose()?
        .flatten();
    let jwt_id = jwt_payload
        .as_ref()
        .map(|v| subject(v, &["id", "user_id", "sub"]))
        .transpose()?
        .flatten();
    if profile_id.is_some() && jwt_id.is_some() && profile_id != jwt_id {
        return Err(QuotaError::IdentityConflict);
    }
    let zcode_subject = profile_id.or(jwt_id).ok_or(QuotaError::IdentityMissing)?;
    let (business_token, business_error) = match read_secret(
        creds,
        &format!("oauth:{}:access_token", family.as_str()),
        secret,
    ) {
        Ok(token) => (token, None),
        Err(error) => (None, Some(error)),
    };
    // Different namespace: never compare this to the ZCode subject.
    let business_subject = business_token
        .as_deref()
        .and_then(zcrypto::decode_jwt)
        .and_then(|v| {
            v.get("user_id")
                .or_else(|| v.get("sub"))
                .and_then(normalized_id)
        });
    Ok(AccountQueryContext {
        scope: AccountScope {
            snapshot_id: snapshot.id.clone(),
            family,
            zcode_subject,
            revision: quota_revision(snapshot, secret),
        },
        auth: AccountAuth {
            zcode_jwt,
            business_token,
            business_error,
            business_subject,
        },
        device_mid: snapshot
            .virtual_device_mid
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .cloned(),
        app_version: app_version.to_string(),
    })
}

#[cfg(test)]
mod tests;

pub(crate) fn login_snapshot(id: &str, credentials: Value, mid: Option<String>) -> Account {
    Account {
        id: id.into(),
        credentials,
        virtual_device_mid: mid,
        name: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
        hash: String::new(),
        config: None,
        virtual_arms_uid: None,
    }
}

pub(crate) fn strict_zcode_jwt(creds: &Value, secret: &str) -> Result<String, QuotaError> {
    let snapshot = login_snapshot("claim", creds.clone(), None);
    let context = resolve_account_context(&snapshot, secret, "")?;
    valid_token(context.auth.zcode_jwt.as_deref()).map(String::from)
}
