use super::*;
use crate::quota_auth::resolve_account_context;
use crate::quota_http::Request;
use crate::test_support::{account, SECRET};
use serde_json::json;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Mutex;

fn context(label: &str, family: &str) -> AccountQueryContext {
    resolve_account_context(&account(label, family), SECRET, "3.16.3").unwrap()
}

fn customer() -> Value {
    json!({"code": 200, "data": {"organizations": [
        {"organizationId":"other", "projects":[{"projectId":"other-personal", "projectType":0}]},
        {"organizationId":"org/a", "organizationName":"默认机构", "projects":[
            {"projectId":"team/one", "projectType":2, "projectName":"Team One"},
            {"projectId":"personal/p", "projectType":"0", "projectName":"默认项目"},
            {"projectId":"team/two", "projectType":"2", "projectName":"Team Two"}
        ]}
    ]}})
}

fn project(team: bool) -> Project {
    Project {
        organization_id: "org/a".into(),
        project_id: if team { "team/one" } else { "personal/p" }.into(),
        label: "Synthetic project".into(),
        team,
    }
}

#[test]
fn location_default_personal_excludes_teams_and_uses_family_auth() {
    for family in ["zai", "bigmodel"] {
        let ctx = context("a", family);
        let discovered = discover(&ctx, &|req: Request| {
            assert_eq!(req.method, "GET");
            assert_eq!(
                req.url,
                format!(
                    "{}/api/biz/customer/getCustomerInfo",
                    ctx.scope.family.origin()
                )
            );
            let token = ctx.business_token().unwrap();
            let expected = if family == "zai" {
                format!("Bearer {token}")
            } else {
                token.into()
            };
            assert!(req.headers.contains(&("Authorization".into(), expected)));
            Ok(customer())
        })
        .unwrap();
        assert_eq!(discovered.personal.unwrap().project_id, "personal/p");
        assert_eq!(discovered.teams.len(), 2);
    }
}

#[test]
fn location_uses_encoded_project_and_existing_key_only() {
    let ctx = context("a", "bigmodel");
    let location = locate_key(&ctx, &project(false), &|req: Request| {
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "https://bigmodel.cn/api/biz/v1/organization/org%2Fa/projects/personal%2Fp/api_keys");
        Ok(json!({"code":"200","data":[{"name":"unrelated","apiKey":"unrelated"}, {"name":"zcode-api-key","apiKey":"existing-id", "usageScene":1}]}))
    }).unwrap();
    assert_eq!(location.api_key_id, "existing-id");
    let calls = Mutex::new(0);
    assert!(matches!(
        locate_key(&ctx, &project(false), &|req: Request| {
            *calls.lock().unwrap() += 1;
            assert_eq!(req.method, "GET", "must not create a long-lived key");
            Ok(json!({"code":0,"data":[]}))
        }),
        Err(QuotaError::ProjectKeyMissing)
    ));
    assert_eq!(*calls.lock().unwrap(), 1);
}

#[test]
fn location_team_key_requires_key_type_and_project_headers() {
    let ctx = context("a", "zai");
    let loc = locate_key(&ctx, &project(true), &|req: Request| {
        assert!(req
            .headers
            .contains(&("Authorization".into(), ctx.business_token().unwrap().into())));
        assert!(req
            .headers
            .contains(&("bigmodel-project".into(), "team/one".into())));
        Ok(json!({"code":0,"data":[
            {"name":"zcode-team-api-key","apiKey":"bad","keyType":1},
            {"name":"zcode-team-api-key","apiKey":"good","keyType":2,"usageScene":1}
        ]}))
    })
    .unwrap();
    assert_eq!(loc.api_key_id, "good");
}

#[test]
fn location_team_skips_malformed_matching_keys_before_valid_candidate() {
    let ctx = context("a", "zai");
    let loc = locate_key(&ctx, &project(true), &|_: Request| {
        Ok(json!({"code":0,"data":[
            {"name":"zcode-team-api-key","keyType":2},
            {"name":"zcode-team-api-key","keyType":2,"apiKey":"   ","usageScene":1},
            {"name":"zcode-team-api-key","keyType":2,"apiKey":"valid-id","usageScene":1}
        ]}))
    })
    .unwrap();
    assert_eq!(loc.api_key_id, "valid-id");
}

#[test]
fn location_legacy_key_scene_does_not_reject_existing_scoped_key() {
    for family in ["zai", "bigmodel"] {
        for team in [false, true] {
            for scene in [json!(-1), json!(3), json!(null), json!(1), json!(2)] {
                let ctx = context("a", family);
                let loc = locate_key(&ctx, &project(team), &|req: Request| {
                    assert_eq!(req.method, "GET");
                    assert!(req.url.starts_with(ctx.scope.family.origin()));
                    Ok(json!({"code":0,"data":[{
                        "name": if team { "zcode-team-api-key" } else { "zcode-api-key" },
                        "keyType": if team { 2 } else { 1 },
                        "apiKey":"existing-own-id", "usageScene":scene,
                    }]}))
                })
                .unwrap();
                assert_eq!(loc.api_key_id, "existing-own-id");
                assert_eq!(loc.project, project(team));
            }
        }
    }
}

#[test]
fn location_ignores_unverified_historical_location_hints() {
    let mut a = account("a", "zai");
    a.credentials["project-key-location:v1:foreign"] = json!(
        json!({"organizationId":"foreign","projectId":"foreign","apiKeyId":"foreign"}).to_string()
    );
    let ctx = resolve_account_context(&a, SECRET, "3.16.3").unwrap();
    let result = discover(&ctx, &|_: Request| Ok(customer())).unwrap();
    assert_eq!(result.personal.unwrap().organization_id, "org/a");
}

#[test]
fn location_team_subscription_checks_membership_and_unknown_enums() {
    let ctx = context("a", "zai");
    for (status, grant, expected) in [
        ("EFFECTIVE", "VALID", true),
        ("EFFECTIVE", "UNASSIGNED", false),
        ("EXPIRED", "VALID", false),
    ] {
        let result = team_subscription(&ctx, &project(true), &|req: Request| {
            assert!(req.url.ends_with("/api/biz/team/subscribe/product/querySubscribeDetail"));
            assert!(req.headers.contains(&("Authorization".into(),format!("Bearer {}",ctx.business_token().unwrap()))));
            assert!(req.headers.contains(&("bigmodel-organization".into(), "org/a".into())));
            Ok(json!({"code":200,"data":{"hasSubscription":true,"status":status,"memberGrantStatus":grant,"productId":"team-coding"}}))
        }).unwrap();
        assert_eq!(result.is_some(), expected);
    }
    assert_eq!(
        team_subscription(&ctx, &project(true), &|_: Request| Ok(
            json!({"code":0,"data":{"hasSubscription":true,"status":"NEW"}})
        )),
        Err(QuotaError::InvalidResponse)
    );
}

fn service() -> (TokenService, Arc<AtomicI64>) {
    let clock = Arc::new(AtomicI64::new(0));
    let cloned = clock.clone();
    (
        TokenService::new(
            Arc::new(move || cloned.load(Ordering::SeqCst)),
            Arc::new(|| 0),
        ),
        clock,
    )
}

fn location(team: bool) -> KeyLocation {
    KeyLocation {
        project: project(team),
        api_key_id: "key/id".into(),
    }
}
fn issued(token: &str) -> Value {
    json!({"code":200,"data":{"enable":true,"accessToken":token,"tokenType":"Bearer","expiresIn":300,"expiresAt":1000}})
}

#[test]
fn token_issuance_contract_success_codes_and_conservative_expiry() {
    for code in [json!(0), json!(200), json!("0"), json!("200")] {
        let (svc, clock) = service();
        clock.store(1000, Ordering::SeqCst);
        let ctx = context("a", "bigmodel");
        svc.activate(&ctx.scope);
        let result = svc
            .resolve(
                &ctx,
                &location(false),
                &|req: Request| {
                    assert_eq!(req.method, "POST");
                    assert!(req.url.ends_with("/key%2Fid/access_tokens"));
                    assert!(req.headers.contains(&(
                        "Authorization".into(),
                        format!("Bearer {}", ctx.business_token().unwrap())
                    )));
                    assert_eq!(
                        req.body,
                        Some(json!({"clientType":"zcode","clientVersion":"3.16.3"}))
                    );
                    clock.store(4000, Ordering::SeqCst);
                    let mut response = issued("pat-a");
                    response["code"] = code.clone();
                    Ok(response)
                },
                None,
            )
            .unwrap();
        assert_eq!(result.expires_at, 301000);
    }
}

#[test]
fn token_cache_isolates_same_family_accounts_and_projects() {
    let (svc, _) = service();
    let a = context("a", "zai");
    let c = context("c", "zai");
    svc.activate(&a.scope);
    svc.activate(&c.scope);
    let count = AtomicUsize::new(0);
    let http = |_: Request| {
        let n = count.fetch_add(1, Ordering::SeqCst);
        Ok(issued(&format!("pat-{n}")))
    };
    let a1 = svc
        .resolve(&a, &location(false), &http, None)
        .unwrap()
        .token;
    let c1 = svc
        .resolve(&c, &location(false), &http, None)
        .unwrap()
        .token;
    let team = svc.resolve(&a, &location(true), &http, None).unwrap().token;
    assert_ne!(a1, c1);
    assert_ne!(a1, team);
    assert_eq!(
        svc.resolve(&a, &location(false), &http, None)
            .unwrap()
            .token,
        a1
    );
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[test]
fn token_same_family_accounts_issue_concurrently_without_sharing_results() {
    let (svc, _) = service();
    let a = context("a", "zai");
    let c = context("c", "zai");
    svc.activate(&a.scope);
    svc.activate(&c.scope);
    let (started, received) = std::sync::mpsc::channel();
    let (release_a, wait_a) = std::sync::mpsc::channel();
    let (release_c, wait_c) = std::sync::mpsc::channel();
    let waits = [Mutex::new(wait_a), Mutex::new(wait_c)];
    std::thread::scope(|threads| {
        let handles: Vec<_> = [&a, &c]
            .into_iter()
            .zip(&waits)
            .map(|(ctx, wait)| {
                let svc = &svc;
                let started = &started;
                threads.spawn(move || {
                    svc.resolve(
                        ctx,
                        &location(false),
                        &|req: Request| {
                            assert!(req.headers.contains(&(
                                "Authorization".into(),
                                format!("Bearer {}", ctx.business_token().unwrap())
                            )));
                            started.send(ctx.scope.snapshot_id.clone()).unwrap();
                            wait.lock().unwrap().recv().unwrap();
                            Ok(issued(&format!("pat-{}", ctx.scope.snapshot_id)))
                        },
                        None,
                    )
                    .unwrap()
                    .token
                })
            })
            .collect();
        let first = received.recv_timeout(std::time::Duration::from_secs(5));
        let second = received.recv_timeout(std::time::Duration::from_secs(5));
        // Release both even if the assertion will fail, so a regression cannot hang CI.
        let _ = release_c.send(());
        let _ = release_a.send(());
        let tokens: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            first.is_ok() && second.is_ok(),
            "different accounts must issue independently"
        );
        assert_ne!(first.unwrap(), second.unwrap());
        assert_eq!(tokens, ["pat-account-a", "pat-account-c"]);
    });
    for ctx in [&a, &c] {
        let cached = svc
            .resolve(
                ctx,
                &location(false),
                &|_: Request| panic!("own token should be cached"),
                None,
            )
            .unwrap();
        assert_eq!(cached.token, format!("pat-{}", ctx.scope.snapshot_id));
    }
}

#[test]
fn token_concurrent_refreshes_share_only_same_scope() {
    let (svc, _) = service();
    let ctx = context("a", "zai");
    svc.activate(&ctx.scope);
    let count = AtomicUsize::new(0);
    let barrier = std::sync::Barrier::new(8);
    std::thread::scope(|threads| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                threads.spawn(|| {
                    barrier.wait();
                    svc.resolve(
                        &ctx,
                        &location(false),
                        &|_: Request| {
                            count.fetch_add(1, Ordering::SeqCst);
                            Ok(issued("same-pat"))
                        },
                        None,
                    )
                    .unwrap()
                    .token
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), "same-pat");
        }
    });
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn token_old_issuance_cannot_return_after_login_update_or_delete() {
    for delete in [false, true] {
        let (svc, _) = service();
        let ctx = context("a", "zai");
        svc.activate(&ctx.scope);
        let (started, rx) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = Mutex::new(wait);
        std::thread::scope(|threads| {
            let pending = threads.spawn(|| {
                svc.resolve(
                    &ctx,
                    &location(false),
                    &|_: Request| {
                        started.send(()).unwrap();
                        wait.lock().unwrap().recv().unwrap();
                        Ok(issued("old-pat"))
                    },
                    None,
                )
            });
            rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            if delete {
                svc.invalidate(&ctx.scope.snapshot_id)
            } else {
                let mut newer = ctx.scope.clone();
                newer.revision = "new-login".into();
                svc.activate(&newer);
            }
            release.send(()).unwrap();
            assert!(matches!(
                pending.join().unwrap(),
                Err(QuotaError::ScopeChanged)
            ));
        });
    }
}

#[test]
fn token_disabled_and_malformed_responses_never_reuse_old_token() {
    for response in [
        json!({"code":0,"data":{"enable":false}}),
        json!({"code":200,"data":{"accessToken":"bad","tokenType":"Basic","expiresIn":300,"expiresAt":1000}}),
    ] {
        let (svc, clock) = service();
        let ctx = context("a", "zai");
        svc.activate(&ctx.scope);
        svc.resolve(
            &ctx,
            &location(false),
            &|_: Request| Ok(issued("old-pat")),
            None,
        )
        .unwrap();
        clock.store(181000, Ordering::SeqCst);
        let result = svc.resolve(
            &ctx,
            &location(false),
            &|_: Request| Ok(response.clone()),
            None,
        );
        assert!(matches!(
            result,
            Err(QuotaError::TokenDisabled | QuotaError::InvalidResponse)
        ));
    }
}

#[test]
fn token_transient_refresh_keeps_same_unexpired_token_without_extending_expiry() {
    for error in [QuotaError::RateLimited, QuotaError::NetworkError] {
        let (svc, clock) = service();
        let ctx = context("a", "zai");
        svc.activate(&ctx.scope);
        svc.resolve(
            &ctx,
            &location(false),
            &|_: Request| Ok(issued("pat-a")),
            None,
        )
        .unwrap();
        clock.store(181000, Ordering::SeqCst);
        let fallback = svc
            .resolve(&ctx, &location(false), &|_: Request| Err(error), None)
            .unwrap();
        assert_eq!(fallback.token, "pat-a");
        assert_eq!(fallback.expires_at, 300000);
        clock.store(300001, Ordering::SeqCst);
        assert!(
            matches!(svc.resolve(&ctx,&location(false),&|_: Request| Err(error),None),Err(e) if e==error)
        );
    }
}

#[test]
fn token_401_reissues_once_and_never_reuses_rejected_token() {
    let (svc, _) = service();
    let ctx = context("a", "bigmodel");
    svc.activate(&ctx.scope);
    let issues = AtomicUsize::new(0);
    let queries = AtomicUsize::new(0);
    let result = svc.fetch(
        &ctx,
        &location(true),
        ProjectEndpoint::Quota,
        &|req: Request| {
            if req.method == "POST" {
                let n = issues.fetch_add(1, Ordering::SeqCst);
                return Ok(issued(&format!("pat-{n}")));
            }
            queries.fetch_add(1, Ordering::SeqCst);
            assert!(req.url.ends_with("/quota/limit?type=2"));
            assert!(req
                .headers
                .contains(&("bigmodel-project".into(), "team/one".into())));
            Err(QuotaError::LoginRequired)
        },
    );
    assert_eq!(result, Err(QuotaError::LoginRequired));
    assert_eq!(issues.load(Ordering::SeqCst), 2);
    assert_eq!(queries.load(Ordering::SeqCst), 2);
}

#[test]
fn token_late_401_does_not_invalidate_new_token() {
    let (svc, _) = service();
    let ctx = context("a", "zai");
    svc.activate(&ctx.scope);
    svc.resolve(
        &ctx,
        &location(false),
        &|_: Request| Ok(issued("old")),
        None,
    )
    .unwrap();
    svc.resolve(
        &ctx,
        &location(false),
        &|_: Request| Ok(issued("new")),
        Some("old"),
    )
    .unwrap();
    let value = svc
        .resolve(
            &ctx,
            &location(false),
            &|_: Request| panic!("late 401 must reuse the replacement"),
            Some("old"),
        )
        .unwrap();
    assert_eq!(value.token, "new");
}

#[test]
fn token_state_poll_prunes_deleted_or_replaced_accounts_only() {
    let (svc, _) = service();
    let a = context("a", "zai");
    let c = context("c", "zai");
    svc.activate(&a.scope);
    svc.activate(&c.scope);
    svc.reconcile(&[(c.scope.snapshot_id.clone(), c.scope.revision.clone())]);
    assert_eq!(svc.assert_current(&a.scope), Err(QuotaError::ScopeChanged));
    assert_eq!(svc.assert_current(&c.scope), Ok(()));
    svc.reconcile(&[(c.scope.snapshot_id.clone(), "changed".into())]);
    assert_eq!(svc.assert_current(&c.scope), Err(QuotaError::ScopeChanged));
}
