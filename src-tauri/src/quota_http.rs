//! Injectable HTTP boundary. Requests/credentials intentionally have no Debug or Serialize.
use crate::quota_auth::QuotaError;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

fn retry_after_ms(value: Option<&str>, now: i64) -> i64 {
    let delay = value.and_then(|value| {
        value
            .trim()
            .parse::<i64>()
            .ok()
            .map(|seconds| seconds.saturating_mul(1000))
            .or_else(|| {
                chrono::DateTime::parse_from_rfc2822(value)
                    .ok()
                    .map(|date| date.timestamp_millis().saturating_sub(now))
            })
    });
    delay.unwrap_or(5_000).clamp(1_000, 300_000)
}

fn cooldowns() -> &'static Mutex<HashMap<String, i64>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn credential_scope(request: &Request) -> String {
    let origin = request.url.split("/api/").next().unwrap_or(&request.url);
    let fields: Vec<_> = request
        .headers
        .iter()
        .filter(|(name, _)| {
            ["authorization", "bigmodel-organization", "bigmodel-project"]
                .contains(&name.to_ascii_lowercase().as_str())
        })
        .collect();
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(origin, fields)).expect("header strings"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_is_bounded_and_supports_http_dates() {
        assert_eq!(retry_after_ms(Some("120"), 0), 120_000);
        assert_eq!(retry_after_ms(Some("999999"), 0), 300_000);
        assert_eq!(
            retry_after_ms(Some("Thu, 01 Jan 1970 00:02:00 GMT"), 0),
            120_000
        );
        assert_eq!(retry_after_ms(Some("nonsense"), 0), 5_000);
    }
}

pub(crate) struct Request {
    pub method: &'static str,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

pub(crate) trait Transport: Sync {
    fn send(&self, request: Request) -> Result<Value, QuotaError>;
}

impl<F: Fn(Request) -> Result<Value, QuotaError> + Sync> Transport for F {
    fn send(&self, request: Request) -> Result<Value, QuotaError> {
        self(request)
    }
}

pub(crate) struct HttpTransport;
impl Transport for HttpTransport {
    fn send(&self, request: Request) -> Result<Value, QuotaError> {
        let scope = credential_scope(&request);
        let now = chrono::Utc::now().timestamp_millis();
        {
            let mut cooldowns = cooldowns().lock().unwrap_or_else(|e| e.into_inner());
            cooldowns.retain(|_, deadline| *deadline > now);
            if cooldowns.contains_key(&scope) {
                return Err(QuotaError::RateLimited);
            }
        }
        // Do not forward credentials across redirects or propagate response bodies.
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(std::time::Duration::from_secs(8))
            .timeout(std::time::Duration::from_secs(20))
            .build();
        let mut req = agent.request(request.method, &request.url);
        for (key, value) in request.headers {
            req = req.set(&key, &value);
        }
        let response = match request.body {
            Some(body) => req.send_json(body),
            None => req.call(),
        };
        match response {
            Ok(response) => {
                let response_time = response
                    .header("Date")
                    .and_then(|date| chrono::DateTime::parse_from_rfc2822(date).ok())
                    .map(|date| date.timestamp());
                let mut raw: Value = response
                    .into_json()
                    .map_err(|_| QuotaError::InvalidResponse)?;
                if request.url.contains("/zcode-plan/billing/balance") {
                    if let (Some(time), Some(data)) = (
                        response_time,
                        raw.get_mut("data").and_then(Value::as_object_mut),
                    ) {
                        data.insert("server_time".into(), Value::from(time));
                    }
                }
                Ok(raw)
            }
            Err(ureq::Error::Status(status, response)) => {
                if status == 429 {
                    let now = chrono::Utc::now().timestamp_millis();
                    let deadline =
                        now.saturating_add(retry_after_ms(response.header("Retry-After"), now));
                    cooldowns()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(scope, deadline);
                }
                Err(match status {
                    401 | 403 => QuotaError::LoginRequired,
                    404 => QuotaError::ProjectUnavailable,
                    429 => QuotaError::RateLimited,
                    500..=599 => QuotaError::NetworkError,
                    _ => QuotaError::InvalidResponse,
                })
            }
            Err(_) => Err(QuotaError::NetworkError),
        }
    }
}

pub(crate) fn successful_data(raw: &Value, require_code: bool) -> Result<&Value, QuotaError> {
    if !raw.is_object() {
        return Err(QuotaError::InvalidResponse);
    }
    let code = raw.get("code").map(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    });
    match code {
        Some(Some(0 | 200)) => {}
        None if !require_code => {}
        Some(Some(401 | 403 | 1006)) => return Err(QuotaError::LoginRequired),
        Some(Some(429 | 3002 | 3008 | 3009 | 3010)) => return Err(QuotaError::RateLimited),
        _ => return Err(QuotaError::InvalidResponse),
    }
    if raw.get("success") == Some(&Value::Bool(false)) {
        return Err(QuotaError::InvalidResponse);
    }
    raw.get("data").ok_or(QuotaError::InvalidResponse)
}
