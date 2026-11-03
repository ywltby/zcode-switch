"""Offline UI regression for the live ("current login") claim path.

Covers the real call chain: the live row binds to the backend revision returned by its
eligibility read, a claim submitted with a stale revision is rejected and never retried
automatically, and a late result cannot mark or render on the replacement login.
Everything is synthetic and external requests are blocked.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const f = window.liveFixture = {
    accounts: [{ id: 'a1', name: 'Saved A', quota_revision: 'r1', has_config: true,
      has_user_info: true, is_active: false, identity: { provider: 'zai', username: 'saved-a' } }],
    liveIdentity: { provider: 'zai', username: 'live-user' },
    liveHash: 'hash-0',
    previewHashOverride: 'hash-stale',
    livePlans: [
      { plan_id: 'plan-live-1', name: 'Live Grant 1', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
      { plan_id: 'plan-live-2', name: 'Live Grant 2', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
    ],
    claimedPlans: [], attempts: [], starts: [], calls: [],
    holdNext: false, pending: null, slowClaimed: false, claimedLiveHash: null,
  };
  const backendRevision = () => 'backend-' + f.liveHash;
  const listeners = new Map(); const callbacks = new Map(); let callback = 0;
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  window.__liveEmit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__liveSetLogin = (hash, username) => {
    f.liveHash = hash;
    f.liveIdentity = { provider: 'zai', username };
  };
  window.__liveReleasePending = () => {
    if (!f.pending) return;
    const p = f.pending; f.pending = null;
    window.__liveEmit('claim://result', {
      ok: true, claimId: p.claimId, accountId: 'live', accountName: 'live-user',
      revision: p.revision, planId: p.planId, planName: p.planName,
      startsAt: null, endsAt: null, effectiveAt: null, serverTime: Date.now(),
    });
  };
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-live-test';
      if (command === 'get_state') {
        return { accounts: structuredClone(f.accounts), language: 'zh',
          zcode_running: false, live_exists: true, live_logged_in: true,
          live_hash: f.liveHash, live_identity: f.liveIdentity,
          zcode_path_ok: true, auto_claim: false, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_preview' || command === 'claim_refresh') {
        await delay(10);
        const plans = args.id === 'live'
          ? structuredClone(f.livePlans).filter((p) => !f.claimedPlans.some((c) => c.plan_id === p.plan_id))
          : [{ plan_id: 'saved-plan', name: 'Saved Grant', starts_at: Date.now() - 60_000 }];
        const liveHash = f.previewHashOverride || f.liveHash;
        return { accountId: args.id, revision: args.id === 'live' ? backendRevision() : args.revision,
          liveHash: args.id === 'live' ? liveHash : undefined, plans };
      }
      if (command === 'claim_current') {
        if (f.slowClaimed) await delay(800);
        return { plans: structuredClone(f.claimedPlans), history: [], legacyHistory: [],
          liveHash: f.claimedLiveHash || f.liveHash };
      }
      if (command === 'claim_start') {
        f.attempts.push({ ...args });
        // 真实后端语义：快照 revision 与当前登录不一致时拒绝提交（从未开始提交）
        await delay(120);
        if (args.id === 'live' && args.revision !== backendRevision()) {
          throw JSON.stringify({ code: 'scope_changed', accountId: 'live',
            revision: backendRevision(), notStarted: true });
        }
        f.starts.push({ ...args });
        const claimId = args.claimId || ('live-claim-' + f.starts.length);
        const summary = { claimId, accountId: args.id, accountName: 'live-user',
          revision: args.revision, planId: args.planId, planName: 'Live Grant',
          status: 'submitting', auto: false };
        if (f.holdNext) {
          f.pending = { claimId, planId: args.planId, planName: summary.planName, revision: args.revision };
        } else {
          f.claimedPlans = [{ plan_id: args.planId, name: summary.planName, status: 'active', effective_at: 0 }];
          setTimeout(() => window.__liveEmit('claim://result', {
            ok: true, claimId, accountId: args.id, accountName: 'live-user',
            revision: args.revision, planId: args.planId, planName: summary.planName,
            startsAt: null, endsAt: null, effectiveAt: null, serverTime: Date.now(),
          }), 0);
        }
        return summary;
      }
      if (command === 'claim_status') return null;
      if (command === 'claim_sessions') return { sessions: [], recovery: { ok: true } };
      if (command === 'claim_cancel') return { phase: 'cancelled', cancelled: true, submissionInFlight: false };
      if (command === 'set_behavior') return;
      throw new Error(`Unexpected synthetic command: ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "live-claim-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 900})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route(
            "**/*",
            lambda route: route.continue_()
            if route.request.url.startswith("http://127.0.0.1:5173")
            else route.abort(),
        )
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")

        expect(page.locator(".live-slot")).to_have_count(1)

        # 0) 查询开始前后端已切换（界面尚未更新）：结果不得显示、不得采用其 revision、
        #    更不得据此发起领取。
        page.wait_for_function(
            "liveFixture.calls.filter(c => c.command === 'claim_preview' && c.args.id === 'live').length >= 1"
        )
        page.wait_for_timeout(400)
        assert page.locator(".claim-strip.live .btn-claim").count() == 0, (
            "过期快照的资格不得显示领取按钮"
        )
        assert page.evaluate(
            "document.querySelector('.claim-strip.live')?.innerText.includes('Live Grant 1') ?? false"
        ) is False, "过期快照的资格不得显示"
        assert page.evaluate("liveFixture.attempts.length") == 0, "过期快照不得启动领取"

        # 界面切到真实登录后重新查询，才接受结果并绑定 revision
        page.evaluate(
            "liveFixture.previewHashOverride = null; __liveSetLogin('hash-1', 'live-user'); __liveEmit('state-changed', {})"
        )
        page.wait_for_function("liveFixture.calls.filter(c => c.command === 'get_state').length >= 2")
        page.wait_for_function(
            "liveFixture.calls.filter(c => c.command === 'claim_preview' && c.args.id === 'live').length >= 2",
            timeout=8000,
        )
        page.wait_for_function("document.querySelectorAll('.claim-strip.live .btn-claim').length >= 1")
        live_strip = page.locator(".claim-strip.live").first
        expect(live_strip).to_contain_text("Live Grant 1")

        # 1) 资格查询只带 id，并把行绑定到后端返回的真实代次
        preview_args = page.evaluate(
            "liveFixture.calls.find(c => c.command === 'claim_preview' && c.args.id === 'live').args"
        )
        assert "revision" not in preview_args, f"live 资格查询不得携带 revision：{preview_args}"

        # 2) 登录在提交前切换：后端按旧 revision 拒绝，前端不得自动重试旧 plan
        live_strip.locator(".btn-claim").click()
        page.wait_for_function("liveFixture.attempts.length === 1")
        page.evaluate("__liveSetLogin('hash-2', 'live-user-2'); __liveEmit('state-changed', {})")
        page.wait_for_function("liveFixture.calls.filter(c => c.command === 'get_state').length >= 2")
        page.wait_for_timeout(1200)
        attempts = page.evaluate("liveFixture.attempts")
        assert len(attempts) == 1, f"账号切换后不得自动重试旧 plan，实际提交尝试：{attempts}"
        assert attempts[0]["revision"] == "backend-hash-1", attempts[0]
        assert page.evaluate("liveFixture.starts.length") == 0, "被拒绝的提交不得进入提交阶段"

        # 3) 新登录重新查询后用新代次领取成功
        page.wait_for_function(
            "liveFixture.calls.filter(c => c.command === 'claim_preview' && c.args.id === 'live').length >= 2",
            timeout=8000,
        )
        page.wait_for_function("document.querySelectorAll('.claim-strip.live .btn-claim').length >= 1")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("liveFixture.starts.length === 1", timeout=8000)
        start_args = page.evaluate("liveFixture.starts[0]")
        assert start_args["revision"] == "backend-hash-2", f"领取必须绑定当前代次：{start_args}"
        page.wait_for_function(
            "liveFixture.calls.filter(c => c.command === 'claim_current' && c.args.id === 'live').length >= 1",
            timeout=8000,
        )
        expect(page.locator(".claim-strip.claimed")).to_contain_text("Live Grant")

        # 4) 迟到成功结果不得写标记/刷新新登录
        page.evaluate("liveFixture.holdNext = true")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("liveFixture.starts.length === 2", timeout=8000)
        before = page.evaluate(
            "liveFixture.calls.filter(c => c.command === 'claim_current' && c.args.id === 'live').length"
        )
        page.evaluate("__liveSetLogin('hash-3', 'live-user-3'); __liveEmit('state-changed', {})")
        page.wait_for_function("liveFixture.calls.filter(c => c.command === 'get_state').length >= 3")
        page.evaluate("__liveReleasePending()")
        page.wait_for_timeout(1200)
        after = page.evaluate(
            "liveFixture.calls.filter(c => c.command === 'claim_current' && c.args.id === 'live').length"
        )
        assert after == before, ("迟到结果不得刷新新登录的已领取状态", before, after)
        assert page.evaluate("document.querySelectorAll('.claim-strip.claimed').length") == 0, (
            "迟到结果不得把旧登录的领取画到新登录上"
        )
        expect(page.locator(".live-slot")).to_have_count(1)

        # 5) 已领取查询在途换登录：迟到的查询结果不得写入新登录
        page.evaluate("liveFixture.slowClaimed = true")
        page.wait_for_function("document.querySelectorAll('.claim-strip.live .btn-claim').length >= 1")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("liveFixture.starts.length === 3", timeout=8000)
        before_query = page.evaluate(
            "liveFixture.calls.filter(c => c.command === 'claim_current' && c.args.id === 'live').length"
        )
        page.evaluate("__liveSetLogin('hash-4', 'live-user-4'); __liveEmit('state-changed', {})")
        page.wait_for_function("liveFixture.calls.filter(c => c.command === 'get_state').length >= 4")
        page.wait_for_timeout(1400)
        assert page.evaluate(
            "liveFixture.calls.filter(c => c.command === 'claim_current' && c.args.id === 'live').length"
        ) >= before_query, "查询仍会发起"
        assert page.evaluate("document.querySelectorAll('.claim-strip.claimed').length") == 0, (
            "在途的已领取查询返回时登录已切换，结果必须丢弃"
        )

        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        page.screenshot(path=str(output / "live-claim.png"), full_page=True)
        assert not errors, errors
        browser.close()
    print(
        "PASS: live claims bind to the backend revision, a login switch rejects the old "
        "plan without retry, and late events/queries never mark or render on the new login."
    )


if __name__ == "__main__":
    main()
