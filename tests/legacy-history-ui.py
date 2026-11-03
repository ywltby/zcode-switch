"""Offline UI regressions for unattributed backend claim history.

Covers the real call chain:
- startup check failure / delay keeps automatic claiming blocked with a visible error;
- attribution pins the confirmed snapshot, returns plan ids, and those plans stay
  blocked even when the upstream preview keeps returning them;
- attribution rejected because the account changed keeps records and protection;
- a failed clear keeps marks/counts and the block.
Only synthetic accounts, a local mock and localStorage are used.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const f = window.legacyFixture = {
    accounts: [{ id: 'a1', name: 'Saved A', quota_revision: 'r1', has_config: true,
      has_user_info: true, is_active: false,
      identity: { provider: 'zai', username: 'user-a', scope: 'scope-a' } }],
    calls: [], starts: [], legacyHistoryCount: 1,
    failAdopt: false, adoptReject: false, countFail: false, countDelay: 0, dropFail: false,
    adoptDelay: 0,
    plans: [{ plan_id: 'p', name: 'Synthetic grant', starts_at: Date.now() - 60_000,
      ends_at: Date.now() + 3_600_000 }],
    planIds: ['p'], skipped: 0,
  };
  const listeners = new Map();
  const callbacks = new Map();
  let callback = 0;
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const emit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__legacyEmit = emit;
  window.__legacySetAccount = (revision, scope, username) => {
    f.accounts = [{ id: 'a1', name: 'Saved A', quota_revision: revision, has_config: true,
      has_user_info: true, is_active: false,
      identity: { provider: 'zai', username, scope } }];
  };
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-legacy-test';
      if (command === 'get_state') {
        return { accounts: structuredClone(f.accounts), language: 'zh', auto_claim: false,
          activity_probe: false, claim_at_effective: false, zcode_running: false,
          live_exists: false, live_logged_in: false, zcode_path_ok: true, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_preview' || command === 'claim_refresh') {
        await delay(10);
        return { accountId: args.id, revision: args.revision, plans: structuredClone(f.plans) };
      }
      if (command === 'claim_current') {
        return { plans: [], history: [], legacyHistory: [], revision: args.revision,
          identityScope: f.accounts[0].identity.scope };
      }
      if (command === 'claim_legacy_count') {
        const countDelay = Number(window.__legacyCountDelay) || f.countDelay;
        if (countDelay) await delay(countDelay);
        // 启动标志在 mock 之后注册，因此在调用时才读取
        if (f.countFail || window.__legacyCountFail === true) throw 'synthetic legacy check failure';
        const count = f.legacyHistoryCountOverride ?? (window.__legacyCountZero ? 0 : f.legacyHistoryCount);
        return (args.ids || []).map((id) => ({ accountId: id, count }));
      }
      if (command === 'claim_adopt_legacy_history') {
        if (f.adoptDelay) await delay(f.adoptDelay);
        if (f.failAdopt) throw 'synthetic history write failure';
        if (f.adoptReject) throw JSON.stringify({ code: 'scope_changed', accountId: args.id,
          revision: f.accounts[0].quota_revision, notStarted: true });
        f.legacyHistoryCount = f.skipped;
        return { planIds: structuredClone(f.planIds), skipped: f.skipped };
      }
      if (command === 'claim_drop_legacy_history') {
        if (f.dropFail) throw 'synthetic history drop failure';
        f.legacyHistoryCount = 0;
        return 1;
      }
      if (command === 'claim_start') {
        f.starts.push({ ...args });
        const claimId = args.claimId;
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

ROUTE = lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort()


def open_page(browser, init=None):
    page = browser.new_page(viewport={"width": 680, "height": 900})
    page.add_init_script(MOCK)
    if init:
        page.add_init_script(init)
    page.route("**/*", ROUTE)
    page.goto("http://127.0.0.1:5173")
    page.wait_for_load_state("networkidle")
    page.wait_for_function(
        "legacyFixture.calls.filter(c => c.command === 'claim_sessions').length >= 1"
    )
    return page


def refresh(page):
    page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
    page.wait_for_function(
        "legacyFixture.calls.filter(c => c.command === 'claim_refresh').length >= 1"
    )


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "legacy-history-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)

        # 无套餐 ID 的历史归属后仍未处理，提示和自动领取限制应继续存在。
        page = open_page(browser, "try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(1)
        page.evaluate("legacyFixture.skipped = 1; legacyFixture.planIds = []")
        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_function("legacyFixture.calls.some(c => c.command === 'claim_adopt_legacy_history')")
        expect(page.locator(".legacy-note")).to_have_count(1)
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "未归属记录仍存在时不得自动领取"
        page.locator('.legacy-note button:has-text("不是当前登录")').first.click()
        expect(page.locator(".legacy-note")).to_have_count(0)
        page.close()

        # 换登录后重新读取旧历史期间，不能沿用上一个登录的「无旧记录」。
        page = open_page(browser, "window.__legacyCountZero = true; try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(0)
        checks = page.evaluate("legacyFixture.calls.filter(c => c.command === 'claim_legacy_count').length")
        page.evaluate("window.__legacyCountZero = false; legacyFixture.countDelay = 2500; __legacySetAccount('r2', 'scope-b', 'user-b'); __legacyEmit('state-changed', {})")
        page.wait_for_function("n => legacyFixture.calls.filter(c => c.command === 'claim_legacy_count').length > n", arg=checks)
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "换登录后的旧历史检查完成前不得自动领取"
        page.close()

        # 检查完成且没有旧历史时，等待期间手动触发的资格查询要继续领取。
        page = open_page(browser, "window.__legacyCountZero = true; try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(0)
        checks = page.evaluate("legacyFixture.calls.filter(c => c.command === 'claim_legacy_count').length")
        page.evaluate("legacyFixture.countDelay = 1800; __legacySetAccount('r2', 'scope-b', 'user-b'); __legacyEmit('state-changed', {})")
        page.wait_for_function("n => legacyFixture.calls.filter(c => c.command === 'claim_legacy_count').length > n", arg=checks)
        refresh(page)
        page.wait_for_timeout(500)
        assert page.evaluate("legacyFixture.starts.length") == 0, "检查未完成时不能领取"
        page.wait_for_function("legacyFixture.starts.length === 1", timeout=8000)
        page.close()

        # 1) 归属路径：固定快照、写入计划标记、上游仍返回也不重复领取
        page = open_page(browser, "try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(1)
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "旧历史未归属时不得自动领取"

        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_function("!document.querySelector('.legacy-note')", timeout=8000)
        adopt = page.evaluate(
            "legacyFixture.calls.find(c => c.command === 'claim_adopt_legacy_history').args"
        )
        assert adopt.get("revision") == "r1", f"必须固定账号代次：{adopt}"
        assert adopt.get("identityScope") == "acct:scope-a", f"必须固定身份作用域：{adopt}"
        marks = page.evaluate("localStorage.getItem('zsw-claim-marks') || '{}'")
        assert "acct:scope-a|p" in marks, f"归属的计划必须写入标记：{marks}"

        # 上游仍然返回同一个已归属套餐：不得再次提交
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function(
            "legacyFixture.calls.filter(c => c.command === 'claim_refresh').length >= 2"
        )
        page.wait_for_timeout(1200)
        assert page.evaluate("legacyFixture.starts.length") == 0, (
            "归属后同一套餐必须继续阻止自动重复领取"
        )

        # 新增另一个套餐后仍可领取它（保护不是永久死锁）
        page.evaluate(
            "legacyFixture.plans.push({ plan_id: 'q', name: 'Second', starts_at: Date.now() - 60_000,"
            " ends_at: Date.now() + 3_600_000 })"
        )
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function("legacyFixture.starts.length >= 1", timeout=8000)
        assert page.evaluate("legacyFixture.starts[0].planId") == "q"
        page.close()

        # 2) 归属被后端拒绝（账号在确认过程中变化）：中止、保留记录、继续阻止
        page = open_page(browser, "try { localStorage.setItem('zsw-claim-marks', JSON.stringify({ 'a1|p': Date.now() })); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(1)
        page.evaluate("legacyFixture.adoptReject = true")
        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_timeout(700)
        assert "归属失败" in page.locator("body").inner_text()
        expect(page.locator(".legacy-note")).to_have_count(1)
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "归属被拒后不得放开自动领取"
        page.close()

        # 3) 启动读取失败：保持阻止并给出可理解错误
        page = open_page(
            browser,
            "window.__legacyCountFail = true;"
            " try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}",
        )
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.wait_for_timeout(700)
        body = page.locator("body").inner_text()
        assert "检查失败" in body and "暂停自动领取" in body, "读取失败必须提示并暂停"
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "读取失败时不得自动领取"
        page.close()

        # 4) 清除失败：保留旧状态并继续阻止
        page = open_page(browser, "try { localStorage.setItem('zsw-claim-marks', JSON.stringify({ 'a1|p': Date.now() })); } catch (e) {}")
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.evaluate("legacyFixture.dropFail = true")
        page.locator('.legacy-note button:has-text("不是当前登录")').first.click()
        page.wait_for_timeout(700)
        assert "清除失败" in page.locator("body").inner_text()
        expect(page.locator(".legacy-note")).to_have_count(1)
        marks = page.evaluate("localStorage.getItem('zsw-claim-marks') || '{}'")
        assert "a1|p" in marks, "清除失败必须保留旧标记"
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "清除失败后不得放开自动领取"
        page.close()

        # 5) 只有旧目录、没有旧标记：同样阻止
        page = open_page(
            browser,
            "window.__legacyCountZero = true;"
            " try { localStorage.removeItem('zsw-claim-marks');"
            " localStorage.setItem('zsw-activity-catalog', JSON.stringify({"
            "   'a1|legacy': { accountId: 'a1', planId: 'legacy', title: 'Old only catalog',"
            "     startsAt: Date.now() - 1000, endsAt: Date.now() + 3_600_000, firstSeenAt: 1 } }));"
            "} catch (e) {}",
        )
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.wait_for_timeout(700)
        expect(page.locator(".legacy-note")).to_have_count(1)
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "只有旧目录时也必须阻止自动领取"
        page.close()

        # 6) 归属期间账号从 A 切到 B：后端仍按确认的 A 成功，本地不得迁到 B
        page = open_page(
            browser,
            "try { localStorage.setItem('zsw-claim-marks', JSON.stringify({ 'a1|p': Date.now() })); } catch (e) {}",
        )
        page.on("pageerror", lambda error: errors.append(str(error)))
        expect(page.locator(".legacy-note")).to_have_count(1)
        page.evaluate("legacyFixture.adoptDelay = 800")
        page.locator('.legacy-note button:has-text("归属当前登录")').first.click()
        page.wait_for_timeout(200)
        page.evaluate("__legacySetAccount('r2', 'scope-b', 'user-b'); __legacyEmit('state-changed', {})")
        page.wait_for_timeout(1400)
        body = page.locator("body").inner_text()
        assert "已切换" in body, "归属中途切换必须明确提示中止"
        expect(page.locator(".legacy-note")).to_have_count(1)
        marks = page.evaluate("localStorage.getItem('zsw-claim-marks') || '{}'")
        assert "scope-b" not in marks, f"本地标记不得迁到新身份：{marks}"
        refresh(page)
        page.wait_for_timeout(900)
        assert page.evaluate("legacyFixture.starts.length") == 0, "中止后不得放开自动领取"
        page.close()

        # 7) 启动检查尚未返回（延迟）：期间必须保持阻止，返回后按结果继续
        page = open_page(
            browser,
            "window.__legacyCountDelay = 900;"
            " try { localStorage.removeItem('zsw-claim-marks'); localStorage.removeItem('zsw-activity-catalog'); } catch (e) {}",
        )
        page.on("pageerror", lambda error: errors.append(str(error)))
        refresh(page)
        page.wait_for_timeout(300)
        assert page.evaluate("legacyFixture.starts.length") == 0, "检查未返回时不得自动领取"
        page.wait_for_timeout(1200)
        assert page.evaluate("legacyFixture.starts.length") == 0, "旧历史存在时仍不得自动领取"
        assert page.evaluate(
            "legacyFixture.calls.filter(c => c.command === 'claim_legacy_count').length >= 1"
        )
        page.close()

        browser.close()
    assert not errors, errors
    print(
        "PASS: legacy history/catalog block automatic claiming, attribution pins the snapshot "
        "and keeps plan marks, and read/clear failures keep both records and protection."
    )


if __name__ == "__main__":
    main()
