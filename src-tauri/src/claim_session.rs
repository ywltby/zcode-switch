//! Claim session ownership. At most one active claim, exactly one submission
//! permit, and a durable pre-submit marker so a restart cannot silently re-claim.
//!
//! No Tauri, no network, no rendering. Credentials never leave this module
//! through serialization: only the material getter returns them in-process.

use crate::store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;
pub const COMPLETED_TTL_MS: i64 = 10 * 60 * 1000;
pub const COMPLETED_MAX: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClaimPhase {
    Preparing,
    Verifying,
    Submitting,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}

impl ClaimPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Verifying => "verifying",
            Self::Submitting => "submitting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "preparing" => Self::Preparing,
            "verifying" => Self::Verifying,
            "submitting" => Self::Submitting,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "unknown" => Self::Unknown,
            _ => return None,
        })
    }

    pub fn is_active(self) -> bool {
        matches!(self, Self::Preparing | Self::Verifying | Self::Submitting)
    }
}

/// Immutable claim material captured before the captcha window opens.
pub struct ClaimMaterial {
    pub credentials: Value,
    pub config: Option<Value>,
    pub device_mid: String,
}

pub struct ClaimSession {
    pub claim_id: String,
    pub account_id: String,
    pub account_name: String,
    pub revision: String,
    pub identity_scope: String,
    pub plan_id: String,
    pub plan_name: String,
    pub auto: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub phase: ClaimPhase,
    /// Unknown claim this session explicitly replaces after a manual acknowledgement.
    pub replaces: Option<String>,
    pub material: ClaimMaterial,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimSummary {
    pub claim_id: String,
    pub account_id: String,
    pub account_name: String,
    pub revision: String,
    pub plan_id: String,
    pub plan_name: String,
    pub status: String,
    pub auto: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub manual_retry: bool,
    #[serde(skip)]
    pub identity_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
}

#[derive(Debug, PartialEq)]
pub enum StartError {
    Busy,
}

#[derive(Debug, PartialEq)]
pub enum StartGate {
    /// Allowed to reserve a session; the value is the unknown claim it replaces.
    Allowed(Option<String>),
    Unconfirmed,
}

#[derive(Debug, PartialEq)]
pub enum SubmitError {
    NoSession,
    UnknownClaim,
    NotReady,
    PersistFailed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CancelOutcome {
    pub phase: String,
    pub cancelled: bool,
    pub submission_in_flight: bool,
}

impl ClaimSession {
    fn summary(&self) -> ClaimSummary {
        ClaimSummary {
            claim_id: self.claim_id.clone(),
            account_id: self.account_id.clone(),
            account_name: self.account_name.clone(),
            revision: self.revision.clone(),
            plan_id: self.plan_id.clone(),
            plan_name: self.plan_name.clone(),
            status: self.phase.as_str().to_string(),
            auto: self.auto,
            created_at: self.created_at,
            updated_at: self.updated_at,
            manual_retry: false,
            identity_scope: self.identity_scope.clone(),
            result: None,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct AttemptsFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(default)]
    attempts: Vec<StoredAttempt>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct StoredAttempt {
    claim_id: String,
    account_id: String,
    revision: String,
    scope: String,
    plan_id: String,
    phase: String,
    updated_at: i64,
    #[serde(default, alias = "acknowledged")]
    manual_retry: bool,
    #[serde(default)]
    result: Option<Value>,
}

impl StoredAttempt {
    fn from_session(session: &ClaimSession, phase: ClaimPhase) -> Self {
        Self {
            claim_id: session.claim_id.clone(),
            account_id: session.account_id.clone(),
            revision: session.revision.clone(),
            scope: session.identity_scope.clone(),
            plan_id: session.plan_id.clone(),
            phase: phase.as_str().to_string(),
            updated_at: session.updated_at,
            manual_retry: false,
            result: None,
        }
    }

    fn summary(&self) -> ClaimSummary {
        ClaimSummary {
            claim_id: self.claim_id.clone(),
            account_id: self.account_id.clone(),
            account_name: String::new(),
            revision: self.revision.clone(),
            plan_id: self.plan_id.clone(),
            plan_name: String::new(),
            status: self.phase.clone(),
            auto: false,
            created_at: self.updated_at,
            updated_at: self.updated_at,
            manual_retry: self.manual_retry,
            identity_scope: self.scope.clone(),
            result: self.result.clone(),
        }
    }
}

pub struct ClaimRegistry {
    active: Option<ClaimSession>,
    completed: VecDeque<ClaimSummary>,
    unknown: Vec<ClaimSummary>,
    attempts_path: PathBuf,
    recovery: Option<String>,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

impl ClaimRegistry {
    pub fn load(attempts_path: PathBuf) -> Self {
        let mut registry = Self {
            active: None,
            completed: VecDeque::new(),
            unknown: Vec::new(),
            attempts_path,
            recovery: None,
        };
        let text = match std::fs::read_to_string(&registry.attempts_path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return registry,
            Err(error) => {
                registry.recovery = Some(format!("claim_attempts_unreadable: {error}"));
                return registry;
            }
        };
        let parsed: AttemptsFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(_) => {
                registry.recovery = Some("claim_attempts_corrupt".to_string());
                return registry;
            }
        };
        if parsed.schema_version != SCHEMA_VERSION {
            registry.recovery = Some("claim_attempts_unsupported".to_string());
            return registry;
        }
        let now = now_ms();
        for attempt in parsed.attempts {
            match ClaimPhase::parse(&attempt.phase) {
                Some(ClaimPhase::Submitting) | Some(ClaimPhase::Unknown) => {
                    // A submission marker without a recorded result is unresolved.
                    let mut summary = attempt.summary();
                    summary.status = ClaimPhase::Unknown.as_str().to_string();
                    registry.unknown.push(summary);
                }
                Some(phase) if phase.is_active() => {}
                Some(_) => {
                    if now - attempt.updated_at <= COMPLETED_TTL_MS {
                        registry.completed.push_back(attempt.summary());
                    }
                }
                None => {
                    registry.recovery = Some("claim_attempts_corrupt".to_string());
                }
            }
        }
        registry.prune();
        let _ = registry.persist();
        registry
    }

    pub fn recovery_error(&self) -> Option<&str> {
        self.recovery.as_deref()
    }

    pub fn try_start(&mut self, session: ClaimSession) -> Result<ClaimSummary, StartError> {
        if let Some(active) = &self.active {
            if active.claim_id == session.claim_id {
                return Ok(active.summary());
            }
            if active.phase.is_active() {
                return Err(StartError::Busy);
            }
        }
        let summary = session.summary();
        self.active = Some(session);
        Ok(summary)
    }

    pub fn update_plan_name(&mut self, claim_id: &str, plan_name: &str) -> bool {
        match self.active.as_mut().filter(|s| s.claim_id == claim_id) {
            Some(session) => {
                session.plan_name = plan_name.to_string();
                session.updated_at = now_ms();
                true
            }
            None => false,
        }
    }

    pub fn mark_verifying(&mut self, claim_id: &str) -> bool {
        match self.active.as_mut().filter(|s| s.claim_id == claim_id) {
            Some(session) if session.phase == ClaimPhase::Preparing => {
                session.phase = ClaimPhase::Verifying;
                session.updated_at = now_ms();
                true
            }
            _ => false,
        }
    }

    /// Writes the submission marker before returning the permit. On any failure
    /// the caller must not send the claim request.
    pub fn begin_submit(&mut self, claim_id: &str) -> Result<ClaimMaterial, SubmitError> {
        let material = {
            let session = match self.active.as_ref() {
                Some(session) => session,
                None => return Err(SubmitError::NoSession),
            };
            if session.claim_id != claim_id {
                return Err(SubmitError::UnknownClaim);
            }
            if session.phase != ClaimPhase::Verifying {
                return Err(SubmitError::NotReady);
            }
            ClaimMaterial {
                credentials: session.material.credentials.clone(),
                config: session.material.config.clone(),
                device_mid: session.material.device_mid.clone(),
            }
        };
        if let Some(session) = self.active.as_mut() {
            session.phase = ClaimPhase::Submitting;
            session.updated_at = now_ms();
        }
        if self.persist().is_err() {
            if let Some(session) = self.active.as_mut() {
                session.phase = ClaimPhase::Verifying;
            }
            return Err(SubmitError::PersistFailed);
        }
        Ok(material)
    }

    pub fn finish(&mut self, claim_id: &str, phase: ClaimPhase, result: Option<Value>) -> bool {
        let Some(active) = self.active.take() else {
            return false;
        };
        if active.claim_id != claim_id {
            self.active = Some(active);
            return false;
        }
        let replaced = active.replaces.clone();
        let mut summary = active.summary();
        summary.status = phase.as_str().to_string();
        summary.result = result;
        summary.updated_at = now_ms();
        if matches!(phase, ClaimPhase::Succeeded | ClaimPhase::Failed) {
            // A definite outcome supersedes the unknown attempt it replaced.
            if let Some(old) = replaced {
                self.unknown.retain(|record| record.claim_id != old);
            }
        }
        if phase == ClaimPhase::Unknown {
            self.unknown.push(summary.clone());
        } else {
            self.completed.push_back(summary.clone());
        }
        self.prune();
        if self.persist().is_err() && phase != ClaimPhase::Unknown {
            let mut conservative = summary;
            conservative.status = ClaimPhase::Unknown.as_str().to_string();
            self.unknown.push(conservative);
        }
        true
    }

    pub fn cancel(&mut self, claim_id: &str) -> CancelOutcome {
        let Some(active) = self.active.as_mut() else {
            return CancelOutcome {
                phase: ClaimPhase::Cancelled.as_str().to_string(),
                cancelled: false,
                submission_in_flight: false,
            };
        };
        if active.claim_id != claim_id {
            return CancelOutcome {
                phase: active.phase.as_str().to_string(),
                cancelled: false,
                submission_in_flight: active.phase == ClaimPhase::Submitting,
            };
        }
        match active.phase {
            ClaimPhase::Preparing | ClaimPhase::Verifying => {
                let mut summary = active.summary();
                summary.status = ClaimPhase::Cancelled.as_str().to_string();
                self.active = None;
                self.completed.push_back(summary);
                self.prune();
                let _ = self.persist();
                CancelOutcome {
                    phase: ClaimPhase::Cancelled.as_str().to_string(),
                    cancelled: true,
                    submission_in_flight: false,
                }
            }
            ClaimPhase::Submitting => CancelOutcome {
                phase: ClaimPhase::Submitting.as_str().to_string(),
                cancelled: false,
                submission_in_flight: true,
            },
            phase => CancelOutcome {
                phase: phase.as_str().to_string(),
                cancelled: false,
                submission_in_flight: false,
            },
        }
    }

    pub fn status(&self, claim_id: &str) -> Option<ClaimSummary> {
        if let Some(active) = self.active.as_ref().filter(|s| s.claim_id == claim_id) {
            return Some(active.summary());
        }
        if let Some(found) = self.unknown.iter().find(|s| s.claim_id == claim_id) {
            return Some(found.clone());
        }
        self.completed.iter().find(|s| s.claim_id == claim_id).cloned()
    }

    pub fn sessions(&self) -> Vec<ClaimSummary> {
        let mut out = Vec::new();
        if let Some(active) = &self.active {
            out.push(active.summary());
        }
        out.extend(self.unknown.iter().cloned());
        out.extend(self.completed.iter().cloned());
        out
    }

    /// Grants one explicit manual retry for this unknown attempt. Automatic and
    /// batch claims stay blocked until a new claim reaches a definite outcome.
    pub fn allow_manual_retry(&mut self, claim_id: &str) -> bool {
        let mut changed = false;
        for summary in self.unknown.iter_mut().filter(|s| s.claim_id == claim_id) {
            if !summary.manual_retry {
                summary.manual_retry = true;
                changed = true;
            }
        }
        if changed {
            let _ = self.persist();
        }
        changed
    }

    /// Consumes the one-shot manual permit. Returns the unknown claim it replaces.
    pub fn consume_manual_retry(&mut self, identity_scope: &str, plan_id: &str) -> Option<String> {
        let record = self
            .unknown
            .iter_mut()
            .find(|s| s.manual_retry && s.plan_id == plan_id && s.identity_scope == identity_scope)?;
        record.manual_retry = false;
        record.updated_at = now_ms();
        let replaced = record.claim_id.clone();
        let _ = self.persist();
        Some(replaced)
    }

    /// True when this identity and plan already have an unresolved submission.
    pub fn blocks(&self, identity_scope: &str, plan_id: &str) -> bool {
        self.unknown
            .iter()
            .any(|s| s.plan_id == plan_id && s.identity_scope == identity_scope)
    }

    /// Automatic and batch claims are always refused while a result is unresolved.
    /// A manual claim needs a one-shot acknowledgement from the user.
    pub fn gate(&mut self, mode: &str, identity_scope: &str, plan_id: &str) -> StartGate {
        if !self.blocks(identity_scope, plan_id) {
            return StartGate::Allowed(None);
        }
        if mode != "manual" {
            return StartGate::Unconfirmed;
        }
        match self.consume_manual_retry(identity_scope, plan_id) {
            Some(replaced) => StartGate::Allowed(Some(replaced)),
            None => StartGate::Unconfirmed,
        }
    }

    fn prune(&mut self) {
        let now = now_ms();
        while let Some(front) = self.completed.front() {
            if now - front.updated_at > COMPLETED_TTL_MS {
                self.completed.pop_front();
            } else {
                break;
            }
        }
        while self.completed.len() > COMPLETED_MAX {
            self.completed.pop_front();
        }
    }

    fn persist(&self) -> Result<(), String> {
        let mut attempts: Vec<StoredAttempt> = Vec::new();
        if let Some(active) = self.active.as_ref().filter(|s| s.phase == ClaimPhase::Submitting) {
            attempts.push(StoredAttempt::from_session(active, ClaimPhase::Submitting));
        }
        for summary in &self.unknown {
            attempts.push(StoredAttempt {
                claim_id: summary.claim_id.clone(),
                account_id: summary.account_id.clone(),
                revision: summary.revision.clone(),
                scope: summary.identity_scope.clone(),
                plan_id: summary.plan_id.clone(),
                phase: summary.status.clone(),
                updated_at: summary.updated_at,
                manual_retry: summary.manual_retry,
                result: summary.result.clone(),
            });
        }
        for summary in &self.completed {
            attempts.push(StoredAttempt {
                claim_id: summary.claim_id.clone(),
                account_id: summary.account_id.clone(),
                revision: summary.revision.clone(),
                scope: summary.identity_scope.clone(),
                plan_id: summary.plan_id.clone(),
                phase: summary.status.clone(),
                updated_at: summary.updated_at,
                manual_retry: summary.manual_retry,
                result: summary.result.clone(),
            });
        }
        let body = serde_json::to_string(&AttemptsFile {
            schema_version: SCHEMA_VERSION,
            attempts,
        })
        .map_err(|error| error.to_string())?;
        store::atomic_write(&self.attempts_path, &body)
    }
}

pub fn attempts_path(store_dir: &Path) -> PathBuf {
    store_dir.join("claim-attempts.json")
}

/// Channel + validated ZCode subject digest. Deliberately independent of the
/// mutable access token so a credential refresh cannot unblock an unresolved claim.
pub fn identity_scope(credentials: &Value, secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let family = credentials
        .get("oauth:active_provider")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let subject = crate::quota_auth::strict_zcode_jwt(credentials, secret)
        .ok()
        .and_then(|token| crate::zcrypto::decode_jwt(&token))
        .and_then(|payload| {
            ["id", "user_id", "sub"]
                .iter()
                .find_map(|key| payload.get(*key).and_then(crate::quota_auth::normalized_id))
        })
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "{:x}",
        Sha256::digest(format!("{family}:{subject}").as_bytes())
    )
}

#[cfg(test)]
mod tests;
