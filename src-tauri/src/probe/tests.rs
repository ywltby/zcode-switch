use super::*;
use crate::quota_auth::{resolve_account_context, QuotaError};
use crate::quota_http::Request;
use crate::test_support::{account, quota_response, SECRET};

#[test]
fn scoped_probe_retains_models_and_does_not_turn_unknown_into_no_entitlement() {
    let ctx = resolve_account_context(&account("a", "zai"), SECRET, "3.16.3").unwrap();
    let result = quota::query_context_via(&ctx, &|req: Request| Ok(quota_response(&req)));
    match classify(&result).unwrap() {
        ProbeOutcome::Entitled { models, .. } => assert!(models.contains(&"GLM-5.3".to_string())),
        other => panic!("unexpected probe result: {other:?}"),
    }
    let unavailable = quota::unavailable("a", "r1", QuotaError::InvalidResponse);
    assert_eq!(classify(&unavailable), None);
}
