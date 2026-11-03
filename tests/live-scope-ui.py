"""Offline UI regression: the live claimed-state query must belong to the same backend
snapshot the UI shows.

The backend may already serve another login while the frontend poll has not updated yet.
A response whose `liveHash` is missing or different must be rejected (no marks, no
catalog write), and a matching response must still be accepted.

Synthetic accounts, local mock, blocked network.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright

MOCK = r"""
(() => {
  const f = window.scopeFixture = {
    liveIdentity: { provider: 'zai', username: 'live-user' },
    liveHash: 'hash-1',
    plans: [
      { plan_id: 'plan-1', name: 'Grant One', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
      { plan_id: 'plan-2', name: 'Grant Two', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
      { plan_id: 'plan-3', name: 'Grant Three', starts_at: Date.now() - 60_000, ends_at: Date.now() + 3_600_000 },
    ],
    claimed: [], calls: [], starts: [], responseHash: undefined, claimedLiveHashMode: 'current',
    liveOn: false,
  };
  const listeners = new Map();
  const callbacks = new Map();
  let callback = 0;
  const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  const emit = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  const claimedHash = () => {
    if (f.claimedLiveHashMode === 'omit') return undefined;
    if (f.claimedLiveHashMode === 'other') return 'hash-other';
    return f.liveHash;
  };
  window.__liveEmit = emit;
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback, fn); return callback; },
    async invoke(command, args = {}) {
      f.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-live-scope';
      if (command === 'get_state') {
        return { accounts: [], language: 'zh', auto_claim: false, activity_probe: false,
          claim_at_effective: false, zcode_running: false, live_exists: true,
          live_logged_in: true, live_hash: f.liveHash, live_identity: f.liveIdentity,
          zcode_path_ok: true, hot_switch: false };
      }
      if (command === 'get_account_quota') {
        return { account_id: args.id, revision: args.revision, completion: 'complete',
          refreshed_at: 1800000000000, is_empty: true, items: [], plans: [], errors: [] };
      }
      if (command === 'claim_preview' || command === 'claim_refresh') {
        await delay(10);
        const plans = structuredClone(f.plans).filter((p) => !f.claimed.some((c) => c.plan_id === p.plan_id));
        return { accountId: args.id, revision: args.revision || 'backend-rev',
          liveHash: f.liveHash, plans };
      }
      if (command === 'claim_current') {
        await delay(300);
        return { plans: structuredClone(f.claimed), history: [], legacyHistory: [],
          revision: 'backend-rev', liveHash: claimedHash() };
      }
      if (command === 'claim_start') {
        f.starts.push({ ...args });
        const claimId = args.claimId;
        f.claimed = [{ plan_id: args.planId, name: 'Grant', status: 'active', effective_at: 0 }];
        setTimeout(() => emit('claim://result', {
          ok: true, claimId, accountId: args.id, accountName: 'live-user',
          revision: args.revision, planId: args.planId, planName: 'Grant',
          startsAt: null, endsAt: null, effectiveAt: null, serverTime: Date.now(),
        }), 20);
        return { claimId, accountId: args.id, revision: args.revision, planId: args.planId,
          planName: 'Grant', status: 'submitting', auto: false };
      }
      if (command === 'claim_status') return null;
      if (command === 'claim_sessions') return { sessions: [], recovery: { ok: true } };
      if (command === 'claim_legacy_count') {
        return (args.ids || []).map((id) => ({ accountId: id, count: id === 'live' ? 1 : 0 }));
      }
      if (command === 'claim_cancel') return { phase: 'cancelled', cancelled: true, submissionInFlight: false };
      if (command === 'set_behavior') return;
      throw new Error(`Unexpected synthetic command: ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "live-scope-ui"
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
        # 0) 启动后新出现 live 登录：必须重新建立旧数据检查并显示需要确认的记录
        page.evaluate("scopeFixture.liveOn = true; __liveEmit('state-changed', {})")
        page.wait_for_function(
            "scopeFixture.calls.filter(c => c.command === 'claim_legacy_count').length >= 1",
            timeout=8000,
        )
        page.wait_for_function(
            "document.querySelectorAll('.claim-strip.live .btn-claim').length >= 1"
        )
        expect(page.locator(".legacy-note")).to_have_count(1)

        # 1) 响应里的 liveHash 与界面快照不一致：整条结果丢弃
        page.evaluate("scopeFixture.claimedLiveHashMode = 'other'")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("scopeFixture.starts.length === 1", timeout=8000)
        page.wait_for_timeout(900)
        assert page.evaluate("document.querySelectorAll('.claim-strip.claimed').length") == 0, (
            "liveHash 不一致时不得写入已领取状态"
        )
        catalog = page.evaluate("localStorage.getItem('zsw-activity-catalog') || '{}'")
        assert "live:zai:live-user|" not in catalog, "liveHash 不一致时不得写入活动目录"

        # 2) 响应缺失 liveHash：同样安全拒收
        page.evaluate("scopeFixture.claimedLiveHashMode = 'omit'")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("scopeFixture.starts.length === 2", timeout=8000)
        page.wait_for_timeout(900)
        assert page.evaluate("document.querySelectorAll('.claim-strip.claimed').length") == 0, (
            "缺少 liveHash 时不得写入已领取状态"
        )
        catalog = page.evaluate("localStorage.getItem('zsw-activity-catalog') || '{}'")
        assert "live:zai:live-user|plan-2" not in catalog, "缺少 liveHash 时不得写入活动目录"

        # 3) 匹配的快照仍然正常工作
        page.evaluate("scopeFixture.claimedLiveHashMode = 'current'")
        page.wait_for_function("document.querySelectorAll('.claim-strip.live .btn-claim').length >= 1")
        page.locator(".claim-strip.live .btn-claim").first.click()
        page.wait_for_function("scopeFixture.starts.length === 3", timeout=8000)
        page.wait_for_function(
            "document.querySelectorAll('.claim-strip.claimed').length >= 1",
            timeout=8000,
        )

        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        page.screenshot(path=str(output / "live-scope.png"), full_page=True)
        assert not errors, errors
        browser.close()
    print(
        "PASS: claimed-state responses are rejected unless their liveHash matches the "
        "snapshot the UI shows; a matching snapshot is still accepted."
    )


if __name__ == "__main__":
    main()
