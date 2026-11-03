use crate::i18n::tr;
use crate::quota;
use serde::Serialize;

#[cfg(test)]
mod tests;

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProbeOutcome {
    Entitled {
        entitlements: usize,
        models: Vec<String>,
    },
    NoEntitlement,
    AuthFailed,
}

pub(crate) fn switch_probe(paths: &crate::store::Paths, id: &str) -> Option<ProbeOutcome> {
    let result = crate::store::account_quota(paths, id).ok()?;
    classify(&result)
}

pub(crate) fn classify(result: &quota::QuotaOverview) -> Option<ProbeOutcome> {
    if !result.plans.is_empty() {
        return Some(ProbeOutcome::Entitled {
            entitlements: result.plans.len(),
            models: result.models.clone(),
        });
    }
    if result.completion == "complete" && result.is_empty {
        return Some(ProbeOutcome::NoEntitlement);
    }
    if result
        .errors
        .iter()
        .any(|e| e.code == crate::quota_auth::QuotaError::LoginRequired)
    {
        return Some(ProbeOutcome::AuthFailed);
    }
    None
}

pub(crate) fn probe_message(outcome: &ProbeOutcome) -> String {
    match outcome {
        ProbeOutcome::Entitled { .. } => String::new(),
        ProbeOutcome::NoEntitlement => tr("probe.no_entitlement"),
        ProbeOutcome::AuthFailed => tr("probe.auth_failed"),
    }
}
