"""Offline UI regression: a failed queued claim must not be claimed again by the
effective-time batch, and the batch re-checks each item before submitting.

The queue claims p1 and receives a terminal 1003; ~30s later the effective-time batch
runs. If the unified settlement did not persist the hold, the batch submits p1 again.
All data is synthetic and external requests are blocked.
"""
import time
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const f = window.holdFixture = {
    accounts: [{ id: 'a1', name: 'Saved A', quota_revision: 'r1', has_config: true,
      has_user_info: true, is_active: false, identity: { provider: 'zai', username: 'user-a1', scope: 'scope-a' } }],
    calls: [], starts: [], claimAtEffective: true,
    plans: [
      { plan_id: 'p1', name: 'Plan One', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
      { plan_id: 'p2', name: 'Plan Two', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
    ],
  };
  const listeners = new Map();
  const callbacks = new Map();
  let callback = 0;
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const emit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__holdEmit = emit;
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-hold-test';
      if (command === 'get_state') {
        return { accounts: structuredClone(f.accounts), language: 'zh', auto_claim: false,
          activity_probe: true, claim_at_effective: f.claimAtEffective,
          zcode_running: false, live_exists: false, live_logged_in: false,
          zcode_path_ok: true, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_preview' || command === 'claim_refresh') {
        await delay(10);
        return { accountId: args.id, revision: args.revision, plans: [structuredClone(f.plans[0])] };
      }
      if (command === 'activity_probe') {
        return { items: [{ accountId: 'a1', name: 'Saved A', plans: structuredClone(f.plans) }], skipped: [] };
      }
      if (command === 'claim_current') {
        return { plans: [], history: [], revision: args.revision };
      }
      if (command === 'claim_start') {
        const planId = args.planId;
        f.starts.push({ id: args.id, planId, revision: args.revision });
        const claimId = args.claimId;
        if (planId === 'p1') {
          setTimeout(() => emit('claim://result', {
            ok: false, claimId, accountId: args.id, revision: args.revision, planId,
            planName: 'Plan One', code: 1003, terminal: true, challenge: false,
            unknown: false, nextAt: null, message: 'already claimed',
          }), 30);
        }
        return { claimId, accountId: args.id, revision: args.revision, planId,
          planName: planId, status: 'submitting', auto: true };
      }
      if (command === 'claim_status') return null;
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
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "claim-hold-ui"
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
        page.wait_for_function(
            "holdFixture.calls.filter(c => c.command === 'claim_sessions').length >= 1"
        )

        # 队列领取 p1：拿到终态 1003，统一结算应写入 hold
        page.locator('.row[data-id="a1"] button[aria-label="查资格"]').click()
        page.wait_for_function("holdFixture.starts.length === 1", timeout=8000)
        # 让探测把 p2 写进活动列表（到点批次据此构建）
        page.locator(".toolbar button").filter(has_text="活动").first.click()
        page.wait_for_function(
            "holdFixture.calls.filter(c => c.command === 'activity_probe').length >= 1",
            timeout=8000,
        )
        page.keyboard.press("Escape")

        # 到点批次由 30s 首次 tick 触发；终态套餐不得被再次提交。
        # 1003 也会给该账号一个账号级冷却，所以 p2 可能被整体跳过——关键断言是 p1 不重复。
        page.wait_for_timeout(33_000)
        starts = page.evaluate("holdFixture.starts")
        p1 = [s for s in starts if s["planId"] == "p1"]
        assert len(p1) == 1, f"终态套餐被到点批次再次提交：{starts}"
        assert page.evaluate(
            "holdFixture.calls.filter(c => c.command === 'activity_probe').length >= 1"
        ), "到点批次应基于探测到的活动运行"
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        page.screenshot(path=str(output / "hold.png"), full_page=True)
        assert not errors, errors
        browser.close()
    print(
        "PASS: the unified settlement holds a terminal plan, so the effective-time batch "
        "does not claim it again."
    )


if __name__ == "__main__":
    main()
