"""Offline UI regression for auto-claim admission.

Covers the queue consumer path, not just the helper: a plan already marked as
claimed for that account and a plan whose window has not started must never be
submitted, while an eligible plan still is. All accounts, plans and responses are
synthetic; external requests are blocked.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const account = (id, name) => ({
    id, name, quota_revision: 'r1', has_config: true, has_user_info: true,
    identity: { provider: 'zai', username: `user-${id}` },
  });
  const f = window.eligibilityFixture = {
    accounts: [account('a1', 'Marked'), account('a2', 'Future'), account('a3', 'Eligible'), account('a4', 'Eligible B')],
    calls: [], starts: [], sessions: {}, emptyFor: [],
  };
  const now = Date.now();
  const plansFor = (id) => {
    if (f.emptyFor.includes(id)) return [];
    if (id === 'a2') return [{ plan_id: 'p', name: 'Future grant', starts_at: now + 3600_000, ends_at: now + 7200_000 }];
    return [{ plan_id: 'p', name: 'Synthetic grant', starts_at: now - 60_000, ends_at: now + 3600_000 }];
  };
  const listeners = new Map();
  const callbacks = new Map();
  let callback = 0;
  const emit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-eligibility-test';
      if (command === 'get_state') {
        return { accounts: structuredClone(f.accounts), language: 'zh', auto_claim: false,
          zcode_running: false, live_exists: false, live_logged_in: false, zcode_path_ok: true, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_refresh' || command === 'claim_preview') {
        await new Promise((resolve) => setTimeout(resolve, 20));
        return { accountId: args.id, revision: args.revision, plans: plansFor(args.id), activated: true };
      }
      if (command === 'claim_start') {
        f.starts.push({ ...args });
        const session = { claimId: args.claimId, accountId: args.id, revision: args.revision,
          planId: args.planId, planName: 'Synthetic grant', status: 'submitting' };
        f.sessions[args.claimId] = session;
        // 立刻给出一个终态失败：队列释放、账号进入冷却，后面的排队任务才有机会被消费。
        setTimeout(() => emit('claim://result', {
          ok: false, code: 1002, terminal: true, claimId: args.claimId,
          accountId: args.id, planId: args.planId, planName: 'Synthetic grant',
          revision: args.revision, message: 'event ended',
        }), 30);
        return session;
      }
      if (command === 'claim_status') return f.sessions[args.claimId] || null;
      if (command === 'claim_legacy_count') return [];
      if (command === 'claim_sessions') return { sessions: [], recovery: { ok: true } };
      if (command === 'claim_cancel') return { phase: 'cancelled', cancelled: true, submissionInFlight: false };
      if (command === 'set_behavior') return;
      throw new Error(`Unexpected synthetic command: ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "claim-eligibility-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 900})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        # 持久已领取标记：a1 已经领过 p（模拟上次运行写入的 marks）
        page.add_init_script(
            "try { localStorage.setItem('zsw-claim-marks', JSON.stringify({ 'acct:zai:user-a1|p': Date.now() })); } catch (e) {}"
        )
        page.route(
            "**/*",
            lambda route: route.continue_()
            if route.request.url.startswith("http://127.0.0.1:5173")
            else route.abort(),
        )
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")
        expect(page.locator(".row[data-id]")).to_have_count(4)
        # 等恢复流程结束（claim_sessions 完成后队列才允许启动）
        page.wait_for_function(
            "eligibilityFixture.calls.filter(c => c.command === 'claim_sessions').length >= 1"
        )

        page.locator(".toolbar button").filter(has_text="刷新资格").first.click()
        page.wait_for_function(
            "eligibilityFixture.calls.filter(c => c.command === 'claim_refresh').length === 4",
            timeout=8000,
        )
        # 队列消费者：已标记的 a1 与未开始的 a2 不得提交；a3 提交并占住唯一的领取会话
        page.wait_for_function("eligibilityFixture.starts.length === 1", timeout=8000)
        starts = page.evaluate("eligibilityFixture.starts")
        ids = [item["id"] for item in starts]
        assert ids == ["a3"], f"只允许已开始的未领取账号提交，实际：{starts}"
        assert starts[0]["planId"] == "p", starts[0]
        # 另一个可领账号被排到队列里等待
        page.wait_for_function(
            "document.querySelector('.row[data-id=\"a4\"]').innerText.includes('排队中')",
            timeout=8000,
        )

        # 刷新结果为空 → 撤销该账号尚未开始的排队任务
        page.evaluate("eligibilityFixture.emptyFor = ['a4']")
        page.locator('.row[data-id="a4"] button[aria-label="查资格"]').click()
        page.wait_for_function(
            "eligibilityFixture.calls.filter(c => c.command === 'claim_refresh' && c.args.id === 'a4').length >= 2",
            timeout=8000,
        )
        # 队列间隔过去后也不得再提交 a4：撤销必须真的生效，而不只是界面不显示
        page.wait_for_timeout(7000)
        starts_after = page.evaluate("eligibilityFixture.starts")
        assert [item["id"] for item in starts_after] == ["a3"], (
            "空结果必须撤销排队任务，队列不得继续提交 a4",
            starts_after,
        )
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        page.screenshot(path=str(output / "eligibility.png"), full_page=True)
        assert not errors, errors
        browser.close()
    print(
        "PASS: persisted claim mark and future window block the queue consumer; "
        "an empty eligibility read withdraws the queued job for that account."
    )


if __name__ == "__main__":
    main()
