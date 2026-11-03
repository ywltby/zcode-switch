"""Offline UI regression: a saved account is updated to another login.

Covers the real call chain for claim marks, the activity catalog and the asynchronous
claimed-state query: after the account slot points at a different login, the previous
login's marks must not suppress the new login's claim, the old catalog must not render,
and a claimed-state response that lands after the switch must be dropped.
All data is synthetic and external requests are blocked.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const account = (revision, scope, username) => ({
    id: 'a1', name: 'Saved A', quota_revision: revision, has_config: true,
    has_user_info: true, is_active: false,
    identity: { provider: 'zai', username, scope },
  });
  const f = window.isoFixture = {
    accounts: [account('r1', 'scope-a', 'user-a')],
    calls: [], starts: [], slowClaimed: false, claimedPlans: [], claimedRevision: null,
    legacyHistoryCount: 1, failLegacyAdopt: false,
  };
  const listeners = new Map();
  const callbacks = new Map();
  let callback = 0;
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const emit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__isoSetAccount = (revision, scope, username) => {
    f.accounts = [account(revision, scope, username)];
  };
  window.__isoEmit = emit;
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-identity-test';
      if (command === 'get_state') {
        return { accounts: structuredClone(f.accounts), language: 'zh', auto_claim: false,
          zcode_running: false, live_exists: false, live_logged_in: false,
          zcode_path_ok: true, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_preview' || command === 'claim_refresh') {
        await delay(10);
        return { accountId: args.id, revision: args.revision,
          plans: [{ plan_id: 'p', name: 'Synthetic grant', starts_at: Date.now() - 60_000,
            ends_at: Date.now() + 3_600_000 }] };
      }
      if (command === 'claim_current') {
        if (f.slowClaimed) await delay(800);
        return { plans: structuredClone(f.claimedPlans), history: [], legacyHistory: [],
          revision: f.claimedRevision || f.accounts[0].quota_revision,
          identityScope: f.accounts[0].identity.scope };
      }
      if (command === 'claim_legacy_count') {
        const count = f.legacyHistoryCount;
        return (args.ids || []).map((id) => ({ accountId: id, count }));
      }
      if (command === 'claim_adopt_legacy_history') {
        if (f.failLegacyAdopt) throw 'synthetic history write failure';
        f.legacyHistoryCount = 0;
        return 1;
      }
      if (command === 'claim_drop_legacy_history') {
        f.legacyHistoryCount = 0;
        return 1;
      }
      if (command === 'claim_start') {
        f.starts.push({ ...args });
        const claimId = args.claimId;
        f.claimedPlans = [{ plan_id: args.planId, name: 'Synthetic grant', status: 'active', effective_at: 0 }];
        setTimeout(() => emit('claim://result', {
          ok: true, claimId, accountId: args.id, accountName: 'Saved A',
          revision: args.revision, planId: args.planId, planName: 'Synthetic grant',
          startsAt: null, endsAt: null, effectiveAt: null, serverTime: Date.now(),
        }), 30);
        return { claimId, accountId: args.id, revision: args.revision, planId: args.planId,
          planName: 'Synthetic grant', status: 'submitting', auto: false };
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
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "identity-isolation-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 900})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        # 旧登录留下的领取标记与活动目录（旧作用域 scope-a）
        page.add_init_script(
            "try {"
            " localStorage.setItem('zsw-claim-marks', JSON.stringify({"
            "   'acct:scope-a|p': Date.now(),"   # 按作用域分区的新键
            "   'a1|p': Date.now()"                # 旧版本的账号键（迁移前数据）
            " }));"
            " localStorage.setItem('zsw-activity-catalog', JSON.stringify({"
            "   'acct:scope-a|p': { accountId: 'a1', scope: 'acct:scope-a', planId: 'p',"
            "     campaignId: 'old-campaign', title: 'Old Login Activity',"
            "     startsAt: Date.now() - 1000, endsAt: Date.now() + 3_600_000, firstSeenAt: 1 } }));"
            "} catch (e) {}"
        )
        page.route(
            "**/*",
            lambda route: route.continue_()
            if route.request.url.startswith("http://127.0.0.1:5173")
            else route.abort(),
        )
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")
        expect(page.locator(".row[data-id]")).to_have_count(1)
        page.wait_for_function(
            "isoFixture.calls.filter(c => c.command === 'claim_sessions').length >= 1"
        )

        # 1) 旧身份已标记：刷新后不得提交
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function(
            "isoFixture.calls.filter(c => c.command === 'claim_refresh').length >= 1"
        )
        page.wait_for_timeout(1500)
        assert page.evaluate("isoFixture.starts.length") == 0, "旧身份的领取标记必须生效"

        # 2) 账号保存成另一登录：新身份不受旧标记压制
        page.evaluate("__isoSetAccount('r2', 'scope-b', 'user-b'); __isoEmit('state-changed', {});")
        page.wait_for_function("isoFixture.calls.filter(c => c.command === 'get_state').length >= 2")
        page.wait_for_timeout(300)
        # 旧记录无法归属：默认继续压制，不允许自动重复尝试
        assert page.locator(".legacy-note").count() >= 1, "旧记录必须显示可确认的提示"
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function(
            "isoFixture.calls.filter(c => c.command === 'claim_refresh').length >= 2"
        )
        page.wait_for_timeout(800)
        assert page.evaluate("isoFixture.starts.length") == 0, (
            "未归属的旧标记不得被直接忽略后自动重复领取"
        )

        # 2b) 归属失败：不得提示成功，也不得放开自动领取
        page.evaluate("isoFixture.failLegacyAdopt = true")
        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_timeout(600)
        body = page.locator("body").inner_text()
        assert "归属失败" in body, "归属失败必须明确提示，而不是显示成功"
        assert page.locator(".legacy-note").count() >= 1, "失败后旧记录必须保持未归属"
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_timeout(1200)
        assert page.evaluate("isoFixture.starts.length") == 0, "归属失败后不得放开自动领取"

        # 2c) 用户确认「归属当前登录」：记录迁到新作用域，仍然压制
        page.evaluate("isoFixture.failLegacyAdopt = false")
        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_function("!document.querySelector('.legacy-note')", timeout=8000)
        marks = page.evaluate("localStorage.getItem('zsw-claim-marks') || '{}'")
        assert "acct:scope-b|p" in marks, f"确认后必须写入当前作用域：{marks}"
        assert "a1|p" not in marks, "确认后旧键必须移除"
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_timeout(1200)
        assert page.evaluate("isoFixture.starts.length") == 0, "确认归属后仍然压制自动领取"

        # 2c) 用户声明「不是当前登录」：清除后才允许自动领取重新尝试
        page.evaluate(
            "try { localStorage.setItem('zsw-claim-marks', JSON.stringify({ 'a1|p': Date.now() })); } catch (e) {}"
        )
        page.reload()
        page.wait_for_load_state("networkidle")
        page.wait_for_function("isoFixture.calls.filter(c => c.command === 'claim_sessions').length >= 1")
        page.evaluate("__isoSetAccount('r2', 'scope-b', 'user-b'); __isoEmit('state-changed', {})")
        page.wait_for_timeout(500)
        assert page.locator(".legacy-note").count() >= 1, "旧标记需要再次确认"
        page.locator('.legacy-note button:has-text("不是当前登录")').first.click()
        page.wait_for_function("!document.querySelector('.legacy-note')", timeout=8000)
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function("isoFixture.starts.length >= 1", timeout=8000)
        start = page.evaluate("isoFixture.starts[0]")
        assert start["revision"] == "r2", f"新登录必须用新代次提交：{start}"

        # 3) 已领取查询在途时账号再换成另一登录：结果必须丢弃
        page.evaluate("isoFixture.slowClaimed = true; __isoSetAccount('r3', 'scope-c', 'user-c'); __isoEmit('state-changed', {});")
        page.wait_for_function("isoFixture.calls.filter(c => c.command === 'get_state').length >= 3")
        page.wait_for_timeout(300)
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function("isoFixture.starts.length >= 2", timeout=8000)
        # 领取成功后 handleClaimResult 会发起已领取查询（800ms 才返回）
        page.wait_for_function(
            "isoFixture.calls.filter(c => c.command === 'claim_current').length >= 1",
            timeout=8000,
        )
        page.evaluate("__isoSetAccount('r4', 'scope-d', 'user-d'); __isoEmit('state-changed', {});")
        page.wait_for_function("isoFixture.calls.filter(c => c.command === 'get_state').length >= 4")
        page.wait_for_timeout(1400)
        assert page.locator(".claim-strip.claimed").count() == 0, (
            "在途的已领取查询返回时账号已换成另一登录，结果必须丢弃"
        )
        # 迟到的结果若被写入，会以新作用域写入活动目录——它必须不出现
        catalog = page.evaluate("localStorage.getItem('zsw-activity-catalog') || '{}'")
        assert "acct:scope-d|p" not in catalog, (
            "迟到的已领取结果不得写入新登录的活动目录"
        )

        # 3b) 后端快照已换成另一登录、前端轮询尚未更新：响应侧代次不匹配也必须丢弃
        page.evaluate("isoFixture.slowClaimed = true; isoFixture.claimedRevision = 'r999'")
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function("isoFixture.starts.length >= 3", timeout=8000)
        page.wait_for_function(
            "isoFixture.calls.filter(c => c.command === 'claim_current').length >= 2",
            timeout=8000,
        )
        page.wait_for_timeout(1400)
        catalog = page.evaluate("localStorage.getItem('zsw-activity-catalog') || '{}'")
        assert "acct:scope-d|p" not in catalog, (
            "响应里的 revision 与界面账号不一致时不得写入"
        )

        # 4) 旧登录的活动目录不得出现在新登录下
        page.locator(".toolbar button").filter(has_text="活动").first.click()
        page.wait_for_timeout(400)
        body = page.locator("body").inner_text()
        assert "Old Login Activity" not in body, "旧登录的活动目录不得显示在新登录下"

        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        page.screenshot(path=str(output / "identity.png"), full_page=True)
        assert not errors, errors
        browser.close()
    print(
        "PASS: marks, catalog and in-flight claimed queries are scoped to the real identity; "
        "updating a saved account to another login starts clean."
    )


if __name__ == "__main__":
    main()
