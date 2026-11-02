use super::*;
use crate::test_support::{account, jwt, SECRET};
use serde_json::json;

#[test]
fn claim_never_uses_foreign_config_key_or_conflicting_jwt() {
    let mut a = account("a", "bigmodel");
    assert_eq!(
        claim_token(&a.credentials, a.config.as_ref(), SECRET).unwrap(),
        a.credentials["zcodejwttoken"].as_str().unwrap()
    );
    a.credentials
        .as_object_mut()
        .unwrap()
        .remove("zcodejwttoken");
    assert!(claim_token(&a.credentials, a.config.as_ref(), SECRET).is_err());
    a.credentials["zcodejwttoken"] = json!(jwt(json!({"id":"foreign"})));
    assert!(claim_token(&a.credentials, a.config.as_ref(), SECRET).is_err());
}

fn campaign_body() -> serde_json::Value {
    json!({
        "code": 0,
        "data": {
            "deliveries": [{
                "campaign_id": "1",
                "resource_position": "banner",
                "banner": {
                    "buttons": [
                        { "action": { "type": "close", "args": null } },
                        { "action": { "type": "claim_zcode_plan", "args": { "plan_id": "plan-a" } } }
                    ],
                    "background": { "args": { "zcode_plan": {
                        "plan_id": "plan-a",
                        "name": "CAMPAIGN PLAN",
                        "priority": 111,
                        "entitlements": [{
                            "show_name": "GLM-5.3-Flash",
                            "meter": "model_usage",
                            "unit_type": "token",
                            "grant_units": 300000000.0,
                            "period": "one_time"
                        }]
                    } } },
                    "success_popup": { "buttons": [
                        { "action": { "type": "claim_zcode_plan", "args": { "plan_id": "plan-a" } } }
                    ] }
                }
            }]
        }
    })
}

#[test]
fn campaign_buttons_become_claimable_plans() {
    let plans = parse_marketing_plans(&campaign_body());
    assert_eq!(plans.len(), 1, "one plan, deduplicated across button slots");
    let plan = &plans[0];
    assert_eq!(plan.plan_id, "plan-a");
    assert_eq!(plan.name, "CAMPAIGN PLAN");
    assert_eq!(plan.priority, 111);
    assert_eq!(plan.grant_items[0].name, "GLM-5.3-Flash");
    assert_eq!(plan.grant_items[0].units, 300_000_000.0);
}

#[test]
fn campaign_plan_without_body_falls_back_to_its_id() {
    let body = json!({ "data": { "deliveries": [{
        "popup": { "buttons": [{ "action": { "type": "claim_zcode_plan", "args": { "plan_id": "plan-b" } } }] }
    }] } });
    let plans = parse_marketing_plans(&body);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].name, "plan-b");
    assert!(plans[0].grant_items.is_empty());
}

#[test]
fn campaign_payload_without_claim_action_yields_nothing() {
    for body in [
        json!({ "code": 0, "data": { "deliveries": [] } }),
        json!({ "code": 0, "data": {} }),
        json!({ "code": 0, "msg": "" }),
        json!({ "data": { "deliveries": [{ "banner": { "buttons": [
            { "action": { "type": "navigate", "args": { "plan_id": "x" } } }
        ] } }] } }),
    ] {
        assert!(parse_marketing_plans(&body).is_empty());
    }
}
