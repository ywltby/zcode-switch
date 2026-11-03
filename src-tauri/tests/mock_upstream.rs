
//! claim/changelog 上游 mock 集成测试：锁定协议语义（解析、错误矩阵、缓存 TTL、退避序列）。
//! 全部流量指向 127.0.0.1，断网可跑；env 相关用例经 ENV_LOCK 串行。

mod support;

use serde_json::json;
use support::{
    del_env, mock_creds, set_env, MockUpstream, ProxyGuard, RecordingProxy, Route, TempHome,
    ENV_LOCK,
};
use zcode_switch_lib::claim;

fn mock_account(mid: &str) -> (TempHome, serde_json::Value) {
    let home = TempHome::new("acct");
    let creds = mock_creds();
    let _ = mid;
    (home, creds)
}

#[test]
fn preview_parses_plans_and_entitlements() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(200, r#"{"code":0,"data":{"server_time":100,"plans":[
            {"plan_id":"p1","name":"P One","description":"","priority":5,
             "entitlements":[
                {"entitlement_id":"e1","show_name":"GLM-5.3-Flash","meter":"model_usage","unit_type":"token","grant_units":300000000,"period":"one_time"},
                {"entitlement_id":"e2","show_name":"忽略项","meter":"other","unit_type":"token","grant_units":1,"period":"daily"}
             ]}]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m1");
    let plans = claim::preview_plans(home.path(), &creds, None, Some("11111111-2222-3333-4444-555555555555".into())).unwrap();
    del_env("ZCODE_API_BASE");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].plan_id, "p1");
    // 只保留 meter=model_usage + unit_type=token 的赠送项
    assert_eq!(plans[0].grant_items.len(), 1);
    assert_eq!(plans[0].grant_items[0].units, 300000000.0);
}

#[test]
fn preview_nonzero_code_and_http401_error_out() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![
            (200, r#"{"code":1002,"msg":"event ended"}"#),
            (401, r#"{"code":401,"msg":"unauthorized"}"#),
        ],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m2");
    let e1 = claim::preview_plans(home.path(), &creds, None, None).unwrap_err();
    let e2 = claim::preview_plans(home.path(), &creds, None, None).unwrap_err();
    del_env("ZCODE_API_BASE");
    assert!(e1.contains("event ended"), "got {e1}");
    assert!(e2.contains("401"), "got {e2}");
}

#[test]
fn marketing_extracts_payloads_buttons_and_minimal_entries() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let body = r#"{"code":0,"data":{"server_time":1,"language":"zh-CN","deliveries":[{"campaign_id":"c1","resource_position":"banner","priority":50,"banner":{"buttons":[{"text":{"content":"关闭"},"action":{"type":"close","args":null}},{"text":{"content":"领"},"action":{"type":"claim_zcode_plan","args":{"plan_id":"btn-only-plan"}}}],"background":{"type":"bundle","args":{"zcode_plan":{"plan_id":"zcode-1024","name":"ZCode 1024 Dev Day","priority":101,"entitlements":[{"entitlement_id":"e1","show_name":"GLM-5.3-Flash","meter":"model_usage","unit_type":"token","grant_units":300000000,"period":"one_time"}]}}},"success_popup":{"hero":{"args":{"zcode_plan":{"plan_id":"zcode-1024","name":"ZCode 1024 Dev Day","priority":101,"entitlements":[{"entitlement_id":"e1","show_name":"GLM-5.3-Flash","meter":"model_usage","unit_type":"token","grant_units":300000000,"period":"one_time"}]}}}}}}]}}"#;
    let mock = MockUpstream::start(vec![Route::json("/api/v1/marketing/touch", vec![(200, body)])]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m3");
    let plans = claim::marketing_touch_plans(home.path(), &creds, None, None);
    del_env("ZCODE_API_BASE");
    // 内嵌 payload（zcode-1024，按钮同 id 去重）+ 无 payload 按钮合成最小条目
    assert_eq!(plans.len(), 2, "got {plans:?}");
    let p1024 = plans.iter().find(|p| p.plan_id == "zcode-1024").unwrap();
    assert_eq!(p1024.name, "ZCode 1024 Dev Day");
    assert_eq!(p1024.grant_items.len(), 1);
    let minimal = plans.iter().find(|p| p.plan_id == "btn-only-plan").unwrap();
    assert_eq!(minimal.name, "btn-only-plan");
    assert!(minimal.grant_items.is_empty());
}

/// 用户现场报文（2026-10-26）：405 + provider_code=3012 + unusual activity。
/// 期望：错误上抛（含 405 与 unusual activity 文本），供 risk::observe_upstream_failure 记 strike。
/// 回归：preview 与 marketing 同时下发同一 plan 时，preview 优先去重**不得丢掉活动元数据**。
/// 现场表现就是「活动 ID / 展示位 永远未提供」。
#[test]
fn merge_adopts_campaign_meta_from_marketing() {
    let base = vec![claim::ClaimPlan {
        plan_id: "p9".into(),
        name: "P9".into(),
        description: String::new(),
        priority: 10,
        grants: vec![],
        grant_items: vec![],
        campaign: None,
    }];
    let mut with_meta = base[0].clone();
    with_meta.description = "来自 marketing 的文案".into();
    with_meta.campaign = Some(claim::ActivityCampaign {
        campaign_id: "2101141259813175296".into(),
        position: "banner".into(),
        title: "ZCODE GLOBAL BUILD".into(),
        body: "3亿 Tokens".into(),
        buttons: vec![],
        starts_at: Some(1793012400000),
        ends_at: Some(1793062800000),
    });
    let merged = claim::merge_marketing_plans(base, vec![with_meta]);
    assert_eq!(merged.len(), 1, "同一 plan_id 必须去重");
    let c = merged[0].campaign.as_ref().expect("必须采纳 marketing 的活动元数据");
    assert_eq!(c.campaign_id, "2101141259813175296");
    assert_eq!(c.position, "banner");
    assert_eq!(merged[0].description, "来自 marketing 的文案");
}

/// 探测只允许走 marketing/touch：不得命中 billing/*，也不得写领取历史（只读纪律）。
#[test]
fn probe_only_touches_marketing_and_writes_no_history() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/marketing/touch",
            vec![(
                200,
                r#"{"code":0,"data":{"deliveries":[{"campaign_id":"c1","banner":{"args":{"zcode_plan":{"plan_id":"p1","name":"P1","priority":1,"entitlements":[{"show_name":"GLM","meter":"model_usage","unit_type":"token","grant_units":1,"period":"one_time"}]}}}}]}}"#,
            )],
        ),
        // 故意挂上 preview 路由：若探测误走 billing，下面的断言会失败
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![(200, r#"{"code":0,"data":{"plans":[]}}"#)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mprobe");
    let plans = claim::probe_marketing_only(home.path(), &creds, None, None);
    del_env("ZCODE_API_BASE");

    assert_eq!(plans.len(), 1, "应解析出 1 条活动");
    let paths: Vec<String> = mock.requests().iter().map(|r| r.path.clone()).collect();
    assert!(paths.iter().any(|p| p.contains("/marketing/touch")), "必须请求 marketing：{paths:?}");
    assert!(
        paths.iter().all(|p| !p.contains("/billing/")),
        "探测不得请求 billing 端点，实际：{paths:?}"
    );
    assert!(
        !home.path().join(".zcode-switch").join("claim-history.json").exists(),
        "探测不得写领取历史"
    );
}

/// 活动元数据解析：campaign_id / 展示位 / 文案 / 按钮动作 / 时间窗都要带出来，
/// 否则活动详情弹框只能显示「未提供」。
#[test]
fn marketing_payload_carries_campaign_meta() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/marketing/touch",
        vec![(
            200,
            r#"{"code":0,"data":{"server_time":1,"deliveries":[{
                "campaign_id":"camp-1","resource_position":"banner","priority":9,
                "starts_at":1793013353,"ends_at":1793062800,
                "banner":{
                    "title":"ZCODE GLOBAL BUILD","text":"GLM-5.3-Flash 3亿",
                    "buttons":[{"action":{"type":"claim_zcode_plan","args":{"plan_id":"p9"}}}],
                    "args":{"zcode_plan":{"plan_id":"p9","name":"ZCODE GLOBAL BUILD","priority":9,
                        "entitlements":[{"show_name":"GLM-5.3-Flash","meter":"model_usage","unit_type":"token","grant_units":300000000,"period":"one_time"}]}}
                }}]}}"#,
        )],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mcamp");
    let plans = claim::marketing_touch_plans(home.path(), &creds, None, None);
    del_env("ZCODE_API_BASE");

    let p = plans.iter().find(|p| p.plan_id == "p9").expect("应解析出 p9");
    let c = p.campaign.as_ref().expect("campaign 元数据必须带出");
    assert_eq!(c.campaign_id, "camp-1");
    assert_eq!(c.position, "banner");
    assert_eq!(c.title, "ZCODE GLOBAL BUILD");
    assert!(c.body.contains("3亿"), "文案应带出，实际：{}", c.body);
    assert_eq!(c.buttons.len(), 1);
    assert_eq!(c.buttons[0].kind, "claim_zcode_plan");
    assert_eq!(c.buttons[0].plan_id.as_deref(), Some("p9"));
    assert_eq!(c.starts_at, Some(1793013353000));
    assert_eq!(c.ends_at, Some(1793062800000));
}

#[test]
fn preview_blocked_unusual_activity_3012_surfaces_error() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(405, r#"{"provider_code":3012,"reason":"unknown","status":405,"retryable":false,"message":"request has been blocked due to unusual activity. TraceID: ae8356b0-5a"}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m3012");
    let err = claim::preview_plans(home.path(), &creds, None, None).unwrap_err();
    del_env("ZCODE_API_BASE");
    let lower = err.to_lowercase();
    assert!(err.contains("405"), "风控 HTTP 码必须保留，实际：{err}");
    assert!(lower.contains("unusual activity"), "风控文案必须保留供 strike 判定，实际：{err}");
}

#[test]
fn marketing_empty_deliveries_and_error_silent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/marketing/touch",
        vec![
            (200, r#"{"code":0,"data":{"server_time":1,"language":"zh-CN","deliveries":[]}}"#),
            (200, r#"{"code":3012,"msg":"unusual activity"}"#),
        ],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m4");
    let empty = claim::marketing_touch_plans(home.path(), &creds, None, None);
    let errored = claim::marketing_touch_plans(home.path(), &creds, None, None);
    del_env("ZCODE_API_BASE");
    assert!(empty.is_empty());
    assert!(errored.is_empty(), "marketing 失败必须静默为空");
}

#[test]
fn merge_prefers_preview_and_sorts_by_priority() {
    let base = vec![claim::ClaimPlan {
        plan_id: "p1".into(),
        name: "P1".into(),
        description: String::new(),
        priority: 10,
        grants: vec![],
        grant_items: vec![],
        campaign: None,
    }];
    let extra = vec![
        claim::ClaimPlan { plan_id: "p1".into(), name: "dup".into(), description: String::new(), priority: 99, grants: vec![], grant_items: vec![], campaign: None },
        claim::ClaimPlan { plan_id: "p2".into(), name: "P2".into(), description: String::new(), priority: 50, grants: vec![], grant_items: vec![], campaign: None },
        claim::ClaimPlan { plan_id: "p3".into(), name: "P3".into(), description: String::new(), priority: 1, grants: vec![], grant_items: vec![], campaign: None },
    ];
    let merged = claim::merge_marketing_plans(base, extra);
    let ids: Vec<&str> = merged.iter().map(|p| p.plan_id.as_str()).collect();
    assert_eq!(ids, vec!["p2", "p1", "p3"], "preview 优先去重 + priority 降序");
}

#[test]
fn current_parses_active_plans() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/current",
        vec![(200, r#"{"code":0,"data":{"plans":[
            {"user_plan_id":"u1","plan_id":"zcode-1024","name":"ZCode 1024 Dev Day","status":"active",
             "starts_at":100,"ends_at":200,
             "entitlements":[{"entitlement_id":"e1","show_name":"GLM-5.3","meter":"model_usage","unit_type":"token","grant_units":200000000,"period":"one_time"}]}]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m5");
    let plans = claim::fetch_active_plans(home.path(), &creds, None, None).unwrap();
    del_env("ZCODE_API_BASE");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].status, "active");
    assert_eq!(plans[0].ends_at, Some(200000));
    assert_eq!(plans[0].grant_items[0].units, 200000000.0);
}

#[test]
fn claim_error_matrix_3007_terminal_challenge_success() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::with_headers(
        "/api/v1/zcode-plan/billing/claim",
        vec![
            (200, Vec::new(), r#"{"code":3007,"msg":"captcha failed"}"#),
            (200, Vec::new(), r#"{"code":1003,"msg":"already claimed"}"#),
            (403, vec![("x-aliyun-captcha-verify-param", "challenge")], r#"forbidden"#),
            (200, Vec::new(), r#"{"code":0,"data":{"plan":{"user_plan_id":"u1","plan_id":"p1","status":"active","starts_at":1,"ends_at":2},"server_time":3}}"#),
        ],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("m6");
    let e3007 = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None).unwrap_err();
    assert!(e3007.is_captcha_rejected(), "got {:?}", e3007.code);
    let e1003 = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None).unwrap_err();
    assert!(e1003.is_terminal());
    let echal = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None).unwrap_err();
    assert!(echal.is_challenge(), "403+挑战头必须归为 challenge，got {:?}", echal.code);
    let ok = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None).unwrap();
    del_env("ZCODE_API_BASE");
    assert_eq!(ok.pointer("/data/plan/status"), Some(&json!("active")));
}

/// 请求规格断言：`billing/preview` 必须带成套身份头（缺任一条上游会误判成风控/无资格），
/// 且 GET 不得携带 body（防凭证外泄）。
#[test]
fn preview_requests_carry_identity_headers() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(200, r#"{"code":0,"data":{"server_time":1,"plans":[]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mhdr");
    let _ = claim::preview_plans(
        home.path(),
        &creds,
        None,
        Some("11111111-2222-3333-4444-555555555555".into()),
    )
    .unwrap();
    del_env("ZCODE_API_BASE");

    let req = mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/billing/preview"))
        .expect("preview 请求未到达 mock");
    let required = [
        "authorization",
        "x-device-mid",
        "x-zcode-app-version",
        "x-platform",
        "x-os-category",
        "x-client-language",
        "x-client-timezone",
        "x-title",
        "user-agent",
    ];
    for k in required {
        assert!(
            req.headers.contains_key(k),
            "缺少请求头 {k}，实际携带：{:?}",
            req.headers.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        !req.headers.get("x-zcode-app-version").unwrap().is_empty(),
        "X-ZCode-App-Version 不能为空（版本门槛会过滤活动）"
    );
    assert!(req.body.is_empty(), "GET 请求不应有 body，实际：{}", req.body);
}

/// 请求规格断言：领取 POST 的 body 只允许 `plan_id`，验证码走 header（参数与地区成套）。
#[test]
fn claim_post_sends_plan_id_and_captcha_headers() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::with_headers(
        "/api/v1/zcode-plan/billing/claim",
        vec![(
            200,
            Vec::new(),
            r#"{"code":0,"data":{"plan":{"user_plan_id":"u1","plan_id":"p1","status":"active","starts_at":1,"ends_at":2},"server_time":3}}"#,
        )],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mbody");
    claim::submit_claim(home.path(), &creds, None, "p1", "cap-x", Some("cn"), None).unwrap();
    del_env("ZCODE_API_BASE");

    let req = mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/billing/claim"))
        .expect("claim 请求未到达 mock");
    assert_eq!(
        req.headers.get("x-aliyun-captcha-verify-param").map(String::as_str),
        Some("cap-x"),
        "验证码参数必须走 header"
    );
    assert_eq!(
        req.headers.get("x-aliyun-captcha-verify-region").map(String::as_str),
        Some("cn"),
        "验证码地区必须成套发送（缺失会导致 3007）"
    );
    let body: serde_json::Value = serde_json::from_str(&req.body).expect("claim body 必须是 JSON");
    let obj = body.as_object().expect("claim body 必须是对象");
    assert_eq!(obj.len(), 1, "body 只允许 plan_id，实际：{}", req.body);
    assert_eq!(obj.get("plan_id").and_then(|v| v.as_str()), Some("p1"));
    assert!(!req.body.contains("Bearer"), "凭证不得进入 body");
}

#[test]
fn captcha_config_uses_mock_then_falls_back_to_default() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    claim::reset_captcha_config_cache_for_tests();
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/client/configs",
        vec![(200, r#"{"code":0,"data":{"configs":{"captcha":{"enabled":true,"region":"cn","prefix":"mockpx","sceneId":"mockscene"}}}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let cfg = claim::fetch_captcha_config().unwrap();
    del_env("ZCODE_API_BASE");
    assert_eq!(cfg.prefix, "mockpx");
    // 缓存命中（不重发请求）→ 改 env 到不可达端口也应返回缓存值
    set_env("ZCODE_API_BASE", "http://127.0.0.1:1");
    let cached = claim::fetch_captcha_config().unwrap();
    assert_eq!(cached.prefix, "mockpx");
    // 清缓存后请求失败 → 内置默认兜底，流程不中断
    claim::reset_captcha_config_cache_for_tests();
    let def = claim::fetch_captcha_config().unwrap();
    del_env("ZCODE_API_BASE");
    assert_eq!(def.prefix, "no8xfe");
    assert_eq!(def.scene_id, "11xygtvd");
    claim::reset_captcha_config_cache_for_tests();
}

#[test]
fn changelog_refresh_ttl_and_stale_refetch() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/changelog",
        vec![
            (200, r#"<h2>Release v3.16.3</h2><h2>Release v3.15.1</h2>"#),
            (200, r#"<h2>Release v3.17.0</h2>"#),
        ],
    )]);
    let home = TempHome::new("chlog");
    set_env("ZCODE_CHANGELOG_URL", &format!("{}/changelog", mock.base));
    crate_changelog_refresh(home.path());
    assert_eq!(crate_changelog_cached(home.path()).as_deref(), Some("3.16.3"));
    // TTL 内不重发请求：mock 第二条响应未被消费（hits==1）
    crate_changelog_refresh(home.path());
    assert_eq!(mock.hits(0), 1, "24h TTL 内不得重复请求");
    // 缓存过期 → 重新拉取到新版本
    make_cache_stale(home.path());
    crate_changelog_refresh(home.path());
    del_env("ZCODE_CHANGELOG_URL");
    assert_eq!(crate_changelog_cached(home.path()).as_deref(), Some("3.17.0"));
}

#[test]
fn changelog_bad_html_falls_back_silently() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json("/changelog", vec![(200, "<p>no versions here 1024 2026-10-23</p>")])]);
    let home = TempHome::new("chlog-bad");
    set_env("ZCODE_CHANGELOG_URL", &format!("{}/changelog", mock.base));
    crate_changelog_refresh(home.path());
    del_env("ZCODE_CHANGELOG_URL");
    assert_eq!(crate_changelog_cached(home.path()), None, "解析失败必须静默回退");
}

// —— changelog 模块经 pub API 暴露的薄包装（保持集成测试面最小）——
fn crate_changelog_refresh(home: &std::path::Path) {
    zcode_switch_lib::changelog::refresh_cache(home);
}
fn crate_changelog_cached(home: &std::path::Path) -> Option<String> {
    zcode_switch_lib::changelog::cached_latest(home)
}

fn make_cache_stale(home: &std::path::Path) {
    let file = home.join(".zcode-switch").join("changelog-cache.json");
    let raw = std::fs::read_to_string(&file).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let stale = json!({
        "version": v["version"],
        "fetched_at": 0,
    });
    std::fs::write(&file, stale.to_string()).unwrap();
}

/// 历史版本会把 config 里的 provider Key 当作领取凭据兜底：旧配置混入其他账号的
/// Key 时，领取就会以别人的身份打到上游。领取只允许使用当前账号自己的 ZCode JWT。
#[test]
fn submit_claim_uses_only_this_accounts_own_jwt() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/claim",
        vec![(
            200,
            r#"{"code":0,"data":{"plan":{"user_plan_id":"u1","plan_id":"p1","status":"active","starts_at":1,"ends_at":2},"server_time":3}}"#,
        )],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("misolation");
    let own_jwt = creds["zcodejwttoken"].as_str().unwrap().to_string();
    // 混入「其他账号」看起来可用的 Key（旧实现会把它当领取凭据）
    let config = json!({
        "provider": {
            "builtin:zai:start-plan": {
                "enabled": true,
                "options": { "apiKey": "foreign-account-api-key-000000000000" }
            }
        }
    });
    claim::submit_claim(home.path(), &creds, Some(&config), "p1", "cap", None, None)
        .expect("当前账号 JWT 有效时领取应成功");
    del_env("ZCODE_API_BASE");

    let req = mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/billing/claim"))
        .expect("claim 请求未到达 mock");
    let auth = req.headers.get("authorization").cloned().unwrap_or_default();
    assert_eq!(auth, format!("Bearer {own_jwt}"), "领取必须只使用当前账号 JWT");
    assert!(
        !auth.contains("foreign-account"),
        "不得使用配置里其他账号的 Key 领取，实际：{auth}"
    );
}

/// 缺少当前账号 JWT 时，配置里的其他账号 Key 不能顶上：既不查询也不领取，
/// 且一个上游请求都不发（确定失败，不是「结果不确定」）。
#[test]
fn claim_without_own_jwt_never_falls_back_to_configured_keys() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(200, r#"{"code":0,"data":{"plans":[]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("nnoauth");
    // 保留 provider 身份，只移除本账号的 JWT
    let mut creds = creds;
    creds.as_object_mut().unwrap().remove("zcodejwttoken");
    let config = json!({
        "provider": {
            "builtin:zai:start-plan": {
                "enabled": true,
                "options": { "apiKey": "foreign-account-api-key-000000000000" }
            }
        }
    });
    let preview = claim::preview_plans(home.path(), &creds, Some(&config), None);
    let submit = claim::submit_claim(home.path(), &creds, Some(&config), "p1", "cap", None, None);
    del_env("ZCODE_API_BASE");

    assert!(preview.is_err(), "缺当前账号 JWT 时资格查询必须失败");
    let err = submit.expect_err("缺当前账号 JWT 时领取必须失败");
    assert!(!err.unknown, "缺凭据是确定失败，不应标记为结果不确定");
    assert_eq!(mock.requests().len(), 0, "缺当前账号 JWT 时不得发任何上游请求");
}

/// 过期快照 + 旧配置残留：过期 JWT 不得用于查询或领取，也不能退到配置里的其他 Key。
#[test]
fn expired_own_jwt_never_queries_or_claims_with_residual_keys() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(200, r#"{"code":0,"data":{"plans":[]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mexpired");
    let mut creds = creds;
    creds["zcodejwttoken"] = json!("mock.jwt.eyJpZCI6Im1vY2stdXNlciIsImV4cCI6MX0.synthetic-signature-000000");
    let config = json!({
        "provider": {
            "builtin:zai:start-plan": {
                "enabled": true,
                "options": { "apiKey": "foreign-account-api-key-000000000000" }
            }
        }
    });
    let preview = claim::preview_plans(home.path(), &creds, Some(&config), None);
    let submit = claim::submit_claim(home.path(), &creds, Some(&config), "p1", "cap", None, None);
    del_env("ZCODE_API_BASE");

    assert!(preview.is_err(), "过期 JWT 不得用于资格查询");
    assert!(submit.is_err(), "过期 JWT 不得用于领取");
    assert_eq!(mock.requests().len(), 0, "过期快照不得发任何上游请求");
}

/// 代理回归：GUI/CLI 写入的代理槽位必须同时被领取、活动查询和激活上报使用。
/// 领取走注入的 http 目标（代理返回 JSON），激活走固定 https 域名（代理只记录 CONNECT）。
#[test]
fn claim_and_activation_use_the_shared_proxy_slot() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let proxy = RecordingProxy::start(r#"{"code":0,"data":{"server_time":1,"plans":[]}}"#);
    let _guard = ProxyGuard::set(&proxy.url);
    set_env("ZCODE_API_BASE", "http://127.0.0.1:1");
    let (home, creds) = mock_account("proxy");
    let plans = claim::preview_plans(home.path(), &creds, None, None).unwrap();
    let marketing = claim::marketing_touch_plans(home.path(), &creds, None, None);
    let _ = claim::report_activation_events("synthetic-user", "synthetic-mid");
    del_env("ZCODE_API_BASE");
    drop(_guard);

    assert!(plans.is_empty());
    assert!(marketing.is_empty());
    let lines = proxy.request_lines();
    assert!(
        lines.iter().any(|l| l.starts_with("GET http://127.0.0.1:1/api/v1/zcode-plan/billing/preview")),
        "领取资格查询必须经代理，实际：{lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("GET http://127.0.0.1:1/api/v1/marketing/touch")),
        "活动查询必须经同一代理，实际：{lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("POST http://127.0.0.1:1/api/v1/event/report")),
        "激活上报必须经同一代理，实际：{lines:?}"
    );
}

/// 版本保鲜回归：领取/活动请求的 URL 参数与请求头必须使用同一份「最新已知版本」，
/// 该版本取 max(本机探测, 内置下限, changelog 缓存)，且不改变 OAuth 的固定版本。
#[test]
fn claim_requests_carry_the_freshest_version_in_url_and_header() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/zcode-plan/billing/preview",
        vec![(200, r#"{"code":0,"data":{"server_time":1,"plans":[]}}"#)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let version_home = TempHome::new("ver");
    std::fs::create_dir_all(version_home.path().join(".zcode-switch")).unwrap();
    std::fs::write(
        version_home.path().join(".zcode-switch").join("changelog-cache.json"),
        r#"{"version":"9.9.9","fetched_at":9999999999}"#,
    )
    .unwrap();
    set_env("ZCODE_SWITCH_HOME", version_home.path().to_str().unwrap());
    claim::reset_app_version_cache_for_tests();
    let (home, creds) = mock_account("ver");
    let _ = claim::preview_plans(home.path(), &creds, None, None).unwrap();
    del_env("ZCODE_API_BASE");
    del_env("ZCODE_SWITCH_HOME");
    claim::reset_app_version_cache_for_tests();

    let req = mock
        .requests()
        .into_iter()
        .find(|r| r.path.contains("/billing/preview"))
        .expect("preview 请求未到达 mock");
    assert!(
        req.url.contains("app_version=9.9.9"),
        "URL 必须带保鲜后的版本，实际：{}",
        req.url
    );
    assert_eq!(
        req.headers.get("x-zcode-app-version").map(String::as_str),
        Some("9.9.9"),
        "请求头版本必须与 URL 一致"
    );
    assert!(
        req.headers
            .get("user-agent")
            .is_some_and(|ua| ua.contains("ZCode/9.9.9")),
        "User-Agent 也要用同一版本，实际：{:?}",
        req.headers.get("user-agent")
    );
    // OAuth 登录协议保持本仓库固定的 3.11.2，不随活动版本变化
    let oauth_headers = zcode_switch_lib::claim::oauth_probe_version_for_tests();
    assert_eq!(oauth_headers, "3.11.2");
}

/// 结果分类回归：业务码必须完整保留（terminal / nextAt 依它推断），
/// 无法判断结果的情况一律保守记为 unknown，不能当成失败或成功。
#[test]
fn claim_result_classification_preserves_codes_and_marks_uncertain_results_unknown() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![Route::with_headers(
        "/api/v1/zcode-plan/billing/claim",
        vec![
            // 1) 200 但缺业务码
            (200, Vec::new(), r#"{"ok":true,"data":{}}"#),
            // 2) 200 业务码不可识别
            (200, Vec::new(), r#"{"code":"weird","msg":"?"}"#),
            // 3) 403 带业务码 1005（无 captcha 特征）：终态 + 上游时间
            (
                403,
                Vec::new(),
                r#"{"code":1005,"msg":"quota exhausted","data":{"plan":{"ends_at":1700000099}}}"#,
            ),
            // 4) 500 带业务码 3012：保留业务码，但结果不确定
            (500, Vec::new(), r#"{"code":3012,"msg":"unusual activity"}"#),
            // 5) 500 无业务码
            (500, Vec::new(), r#"{"oops":true}"#),
            // 6) 200 业务码 1002：终态失败
            (200, Vec::new(), r#"{"code":1002,"msg":"event ended"}"#),
        ],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mclassify");

    let missing_code = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("缺业务码不能当成功");
    assert!(missing_code.unknown, "有效 JSON 但缺业务码必须记为 unknown");

    let odd_code = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("业务码不可识别不能当成功");
    assert!(odd_code.unknown, "业务码不可识别必须记为 unknown");

    let exhausted = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("1005 是失败");
    assert_eq!(exhausted.code, 1005, "HTTP 403 上的业务码必须保留");
    assert!(exhausted.is_terminal(), "1005 是终态");
    assert!(!exhausted.unknown, "确定业务拒绝不是 unknown");
    assert_eq!(
        exhausted.next_at,
        Some(1_700_000_099_000),
        "1005 必须保留上游给出的时间（秒→毫秒）"
    );

    let risk = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("3012 是失败");
    assert_eq!(risk.code, 3012, "5xx 上的业务码也要保留");
    assert!(risk.unknown, "5xx 上的拒绝不能断言请求未生效");
    assert!(!risk.is_terminal(), "3012 是风控信号，不是终态拒绝");

    let server_error = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("500 无业务码是失败");
    assert!(server_error.unknown);
    assert_eq!(server_error.code, -1);

    let ended = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("1002 是失败");
    assert!(ended.is_terminal());
    assert!(!ended.unknown);
    del_env("ZCODE_API_BASE");

    // 传输错误（连接被拒）：POST 可能已发出，必须按 unknown 处理
    set_env("ZCODE_API_BASE", "http://127.0.0.1:1");
    let transport = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("传输失败不能当成功");
    del_env("ZCODE_API_BASE");
    assert!(transport.unknown, "传输错误必须记为 unknown");
}

/// 风控信号必须保留在错误里并上抛：preview 的业务码与被吞掉的 marketing 结果都要能
/// 触发冷却状态机，否则领取准备阶段会带着风控继续打开验证码。
#[test]
fn preview_and_marketing_risk_codes_are_preserved() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![(200, r#"{"code":3012,"msg":"request blocked due to unusual activity"}"#)],
        ),
        Route::json(
            "/api/v1/marketing/touch",
            vec![(200, r#"{"code":3012,"msg":"request blocked due to unusual activity"}"#)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mrisk");

    let preview = claim::preview_plans(home.path(), &creds, None, None)
        .expect_err("preview 3012 必须失败");
    assert!(
        preview.contains("3012") && preview.to_lowercase().contains("unusual activity"),
        "preview 错误必须保留业务码与上游原因，实际：{preview}"
    );

    let collected = claim::collect_claimable_plans(home.path(), &creds, None, "synthetic-mid".into())
        .expect_err("marketing 风控不能被静默吞掉");
    assert!(
        collected.contains("3012") || collected.to_lowercase().contains("unusual activity"),
        "marketing 风控必须上抛给准备阶段，实际：{collected}"
    );
    del_env("ZCODE_API_BASE");
}

/// 普通 preview 失败仍然降级：本地列表不决定最终资格（beta.11 约定）。
#[test]
fn non_risk_marketing_failures_stay_silent() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![(200, r#"{"code":0,"data":{"plans":[{"plan_id":"p1","name":"Synthetic","grants":[]}],"server_time":1}}"#)],
        ),
        Route::json(
            "/api/v1/marketing/touch",
            vec![(500, r#"{"code":5001,"msg":"internal"}"#)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("msoft");
    let plans = claim::collect_claimable_plans(home.path(), &creds, None, "synthetic-mid".into())
        .expect("非风控的 marketing 失败必须降级");
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].plan_id, "p1");
    del_env("ZCODE_API_BASE");
}

/// 上游有时把风控码放在 provider_code / data.code / error.code 里，而不是顶层 code。
/// 成功响应与非成功（4xx/5xx）响应都必须把它带出来，否则冷却状态机看不到风控。
#[test]
fn risk_code_is_preserved_from_nested_and_non_success_responses() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![
                // 200 但业务码在 provider_code
                (200, r#"{"provider_code":3012,"message":"request blocked due to unusual activity"}"#),
                // 500 且沿用嵌套 code
                (500, r#"{"data":{"code":3012},"message":"unusual activity"}"#),
            ],
        ),
        Route::json(
            "/api/v1/marketing/touch",
            vec![
                (200, r#"{"error":{"code":3012,"message":"unusual activity"}}"#),
            ],
        ),
        Route::json(
            "/api/v1/zcode-plan/billing/claim",
            vec![(500, r#"{"provider_code":3012,"msg":"unusual activity"}"#)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mprovider");

    let first = claim::preview_plans(home.path(), &creds, None, None).expect_err("3012 必须失败");
    assert!(
        first.contains("3012") && first.to_lowercase().contains("unusual activity"),
        "200 响应里的 provider_code 必须保留：{first}"
    );

    let second = claim::preview_plans(home.path(), &creds, None, None).expect_err("3012 必须失败");
    assert!(
        second.contains("3012"),
        "500 响应里的嵌套 code 必须保留：{second}"
    );

    let collected = claim::collect_claimable_plans(home.path(), &creds, None, "mid".into())
        .expect_err("marketing 的嵌套 3012 必须上抛");
    assert!(
        collected.contains("3012") || collected.to_lowercase().contains("unusual activity"),
        "marketing 嵌套错误码必须保留：{collected}"
    );

    let submit = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("提交 3012 必须失败");
    assert_eq!(submit.code, 3012, "提交失败必须保留 provider_code");
    assert!(submit.unknown, "5xx 上的提交结果不确定");
    del_env("ZCODE_API_BASE");
}

/// 外层 code=0 但内层 provider_code=3012：显式风控不能被判成成功。
/// 覆盖 preview / marketing / claim 与 HTTP 200 / 4xx / 5xx 的组合。
#[test]
fn nested_risk_code_inside_a_zero_code_is_never_success() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let body = r#"{"code":0,"provider_code":3012,"message":"unusual activity"}"#;
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![(200, body), (400, body), (500, body)],
        ),
        Route::json(
            "/api/v1/marketing/touch",
            vec![(200, body), (400, body), (500, body)],
        ),
        Route::json(
            "/api/v1/zcode-plan/billing/claim",
            vec![(200, body), (400, body), (500, body)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mnested");

    for status in [200_u16, 400, 500] {
        assert!(
            claim::preview_plans(home.path(), &creds, None, None).is_err(),
            "preview {status} 上的嵌套 3012 必须判为失败"
        );
    }
    // 上面的循环只确认全部失败；再检查文案与提交分类
    let claim_err = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("嵌套 3012 的提交不能当成功");
    assert_eq!(claim_err.code, 3012, "必须保留内层 provider_code");
    let marketing = claim::collect_claimable_plans(home.path(), &creds, None, "mid".into())
        .expect_err("内层 3012 的 marketing 不能被静默吞掉");
    assert!(
        marketing.contains("3012") || marketing.to_lowercase().contains("unusual activity"),
        "marketing 风控必须保留：{marketing}"
    );
    del_env("ZCODE_API_BASE");
}

/// 验证码配置与激活上报遇到明确风控时必须保留信号；普通网络故障仍走既有降级。
#[test]
fn captcha_and_activation_risk_signals_are_preserved() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let risk_body = r#"{"code":0,"provider_code":3012,"message":"unusual activity"}"#;
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/client/configs",
            vec![(200, risk_body), (500, r#"{"code":5001,"msg":"internal"}"#)],
        ),
        Route::json("/api/v1/event/report", vec![(200, risk_body)]),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (_home, _creds) = mock_account("mrisk2");

    claim::reset_captcha_config_cache_for_tests();
    let checked = claim::fetch_captcha_config_checked().expect_err("验证码配置的风控必须上抛");
    assert!(
        checked.contains("3012") || checked.to_lowercase().contains("unusual activity"),
        "验证码配置的风控信号必须保留：{checked}"
    );
    claim::reset_captcha_config_cache_for_tests();
    // 普通 500：仍回退内置默认（既有降级不能被改掉）
    let degraded = claim::fetch_captcha_config().expect("普通故障必须降级");
    assert!(degraded.enabled);
    assert_eq!(degraded.prefix, "no8xfe");

    let activation = claim::report_activation_events("synthetic-user", "synthetic-mid")
        .expect_err("激活上报的风控必须上抛");
    assert!(
        activation.contains("3012") || activation.to_lowercase().contains("unusual activity"),
        "激活上报的风控信号必须保留：{activation}"
    );
    del_env("ZCODE_API_BASE");
}

/// 同一响应里同时有外层通用错误码与显式 3012：3012 必须胜出，不能被遮住。
#[test]
fn explicit_3012_wins_over_a_generic_code() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let body = r#"{"code":5001,"provider_code":3012,"message":"unusual activity"}"#;
    let nested = r#"{"code":5001,"data":{"code":3012},"message":"unusual activity"}"#;
    let mock = MockUpstream::start(vec![
        Route::json(
            "/api/v1/zcode-plan/billing/preview",
            vec![(200, body), (400, body), (500, nested)],
        ),
        Route::json("/api/v1/marketing/touch", vec![(200, body)]),
        Route::json(
            "/api/v1/zcode-plan/billing/claim",
            vec![(200, body), (500, nested)],
        ),
        Route::json(
            "/api/v1/client/configs",
            vec![(200, body), (405, body), (500, nested)],
        ),
    ]);
    set_env("ZCODE_API_BASE", &mock.base);
    let (home, creds) = mock_account("mmulti");

    for status in [200_u16, 400, 500] {
        let err = claim::preview_plans(home.path(), &creds, None, None)
            .expect_err("多码并存的 preview 必须失败");
        assert!(
            err.contains("3012"),
            "preview {status} 必须选出显式 3012，实际：{err}"
        );
    }
    let submitted = claim::submit_claim(home.path(), &creds, None, "p1", "cap", None, None)
        .expect_err("多码并存的提交必须失败");
    assert_eq!(submitted.code, 3012, "提交必须保留显式 3012");
    let collected = claim::collect_claimable_plans(home.path(), &creds, None, "mid".into())
        .expect_err("marketing 的多码必须上抛");
    assert!(collected.contains("3012"), "marketing 必须选出 3012：{collected}");

    claim::reset_captcha_config_cache_for_tests();
    let captcha = claim::fetch_captcha_config_checked().expect_err("验证码配置必须失败");
    assert!(
        captcha.contains("3012"),
        "验证码配置的 200 多码必须选出 3012：{captcha}"
    );
    del_env("ZCODE_API_BASE");
}

/// 验证码配置的风控在 200 / 4xx / 5xx 上都必须被识别（HTTP 错误体里也要解析业务码）。
#[test]
fn captcha_config_risk_is_detected_on_every_status() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let body = r#"{"code":0,"provider_code":3012,"message":"unusual activity"}"#;
    let mock = MockUpstream::start(vec![Route::json(
        "/api/v1/client/configs",
        vec![(200, body), (405, body), (500, body)],
    )]);
    set_env("ZCODE_API_BASE", &mock.base);
    let _account = mock_account("mcaptcha");
    for status in [200_u16, 405, 500] {
        claim::reset_captcha_config_cache_for_tests();
        let err = claim::fetch_captcha_config_checked()
            .expect_err("验证码配置的风控必须上抛");
        assert!(
            err.contains("3012") || err.to_lowercase().contains("unusual activity"),
            "captcha {status} 必须保留风控信号：{err}"
        );
    }
    del_env("ZCODE_API_BASE");
}
