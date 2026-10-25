//! Synthetic fixtures only. Never read the user's credentials or contact an API.
use crate::store::Account;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};

pub const SECRET: &str = "synthetic-test-secret";
pub const FOREIGN_KEY: &str = "synthetic-account-b-historical-api-key";

pub fn jwt(payload: Value) -> String {
    format!(
        "e30.{}.synthetic-signature",
        URL_SAFE_NO_PAD.encode(payload.to_string())
    )
}

pub fn account(label: &str, family: &str) -> Account {
    let mut credentials = json!({
        "oauth:active_provider": family,
        "zcodejwttoken": jwt(json!({"id": format!("zcode-{label}")})),
        "account-provider:bigmodel:historical-b:coding-plan:api-key": FOREIGN_KEY,
    });
    credentials[format!("oauth:{family}:access_token")] =
        json!(jwt(json!({"user_id": format!("business-{label}")})));
    credentials[format!("oauth:{family}:user_info")] =
        json!(json!({"id": format!("zcode-{label}")}).to_string());
    Account {
        id: format!("account-{label}"),
        name: label.into(),
        created_at: String::new(),
        updated_at: String::new(),
        hash: String::new(),
        credentials,
        config: Some(json!({"provider": {
            "builtin:bigmodel:coding-plan:old-b": {"enabled": false, "options": {"apiKey": FOREIGN_KEY}},
            "builtin:zai:start-plan:old": {"enabled": true, "options": {"apiKey": "synthetic-foreign-start-plan-key"}}
        }})),
        virtual_device_mid: Some(format!("synthetic-mid-{label}")),
        virtual_arms_uid: None,
    }
}

pub fn quota_response(request: &crate::quota_http::Request) -> Value {
    let url = &request.url;
    if url.contains("billing/balance") {
        json!({"code":0,"data":{"plans":[{"plan_id":"start-plan","subscription_id":"start-a","status":"active"}],"balances":[{"plan_id":"start-plan","subscription_id":"start-a","show_name":"GLM-5.3","total_units":100,"used_units":10,"period_end":"2030-01-01"}]}})
    } else if url.ends_with("getCustomerInfo") {
        json!({"code":200,"data":{"organizations":[{"organizationId":"org-a","organizationName":"Synthetic organization","projects":[
            {"projectId":"personal","projectType":0,"projectName":"Personal"},
            {"projectId":"team-one","projectType":2,"projectName":"Team One"},
            {"projectId":"team-two","projectType":2,"projectName":"Team Two"}
        ]}]}})
    } else if url.ends_with("/api_keys") {
        json!({"code":200,"data":[{"name":"zcode-api-key","apiKey":"personal-key"},{"name":"zcode-team-api-key","apiKey":"team-key","keyType":2}]})
    } else if url.ends_with("/access_tokens") {
        json!({"code":0,"data":{"enable":true,"accessToken":"synthetic-own-project-token","tokenType":"Bearer","expiresIn":3600,"expiresAt":4102444800_i64}})
    } else if url.ends_with("subscription/list") {
        json!({"code":200,"data":[{"productId":"coding-pro","productName":"Coding Pro","subscriptionId":"personal-sub","status":"VALID","inCurrentPeriod":true,"valid":"2030-01-01"}]})
    } else if url.ends_with("querySubscribeDetail") {
        json!({"code":200,"data":{"hasSubscription":true,"status":"EFFECTIVE","memberGrantStatus":"VALID","productId":"team-coding-pro","productName":"Team Pro","subscribeEndTime":"2030-01-01"}})
    } else if url.contains("quota/limit") {
        json!({"code":200,"data":{"level":"Pro","limits":[{"type":"TOKENS_LIMIT","unit":3,"number":1,"usage":100,"currentValue":20,"nextResetTime":1893456000000_i64}]}})
    } else {
        panic!("unexpected synthetic endpoint")
    }
}
