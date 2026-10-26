"""Offline claim UI acceptance with synthetic accounts and a mocked Tauri boundary.

Run against a local Vite server (http://127.0.0.1:5173). No real account,
credential, captcha, or official API request is made.
"""
import re
import time
from pathlib import Path

from playwright.sync_api import sync_playwright, expect

MOCK = r"""
(() => {
  const accounts = Array.from({ length: 11 }, (_, i) => ({
    id: `account-${i}`, name: `Synthetic ${i}`, quota_revision: 'r1', is_active: i === 0,
    has_config: true, has_user_info: true, identity: { provider: i % 2 ? 'bigmodel' : 'zai', username: `synthetic-${i}` },
  }));
  const plan = (id) => ({ plan_id: 'plan-1', name: 'Synthetic plan', description: 'Synthetic grant',
    grants: ['Synthetic · 100 Token'], grant_items: [{ name: 'Synthetic', units: 100, period: 'one_time' }] });
  const fixture = window.claimFixture = {
    language: 'zh', accounts, calls: [], claimIds: [], acknowledged: [], startCount: 0,
    quota(id, revision) {
      return { account_id: id, revision, completion: 'complete', errors: [], refreshed_at: 1800000000000,
        is_empty: false, items: [], plans: [{ name: 'Synthetic Pro', tier: 'Pro', tier_code: 'pro',
          items: [{ name: 'Synthetic credits', total: 100, used: 20, remaining: 80, percent_used: 20, unit: 'credits' }] }] };
    },
    plans(id) { return id === 'account-6' ? [] : [plan(id)]; },
    revisionOf(id) { return this.accounts.find(a => a.id === id)?.quota_revision || 'r1'; },
    session(claimId, accountId, status) {
      return { claimId, accountId, accountName: this.accounts.find(a => a.id === accountId)?.name || accountId,
        revision: this.revisionOf(accountId), planId: 'plan-1', planName: 'Synthetic plan', status, auto: false };
    },
  };
  const callbacks = new Map(); const listeners = new Map(); let callbackId = 0;
  window.__emitToPage = (event, payload) => {
    const handler = listeners.get(event);
    if (handler != null) callbacks.get(handler)?.({ event, id: handler, payload });
  };
  window.__TAURI_INTERNALS__ = {
    transformCallback(callback) { const id = ++callbackId; callbacks.set(id, callback); return id; },
    async invoke(command, args = {}) {
      fixture.calls.push({ command, args });
      if (command === 'plugin:event|listen') { listeners.set(args.event, args.handler); return 1; }
      if (command === 'plugin:event|unlisten') return;
      if (command === 'get_state') {
        return { accounts: structuredClone(fixture.accounts), language: fixture.language, zcode_running: false,
          live_exists: true, live_logged_in: true, zcode_path_ok: true, auto_claim: false, hot_switch: false };
      }
      if (command === 'app_version') return 'synthetic-test';
      if (command === 'reveal_main') return;
      if (command === 'get_account_quota') return fixture.quota(args.id, args.revision);
      if (command === 'claim_preview' || command === 'claim_refresh') {
        const delay = 300;
        if (args.id === 'account-7' && !fixture.retriedAccount7) {
          await new Promise(resolve => setTimeout(resolve, delay));
          throw 'refresh_failed';
        }
        await new Promise(resolve => setTimeout(resolve, delay));
        return { accountId: args.id, revision: args.revision, plans: fixture.plans(args.id),
          ...(command === 'claim_refresh' ? { activated: true, activationError: null } : {}) };
      }
      if (command === 'claim_start') {
        fixture.startCount += 1;
        fixture.claimIds.push(args.claimId);
        const account = fixture.accounts.find(a => a.id === args.id);
        const summary = { ...fixture.session(args.claimId, args.id, 'verifying'), planName: account?.name || args.id };
        // Emit asynchronously, as the real backend does after the captcha window.
        const emit = (extra) => setTimeout(() => window.__emitToPage('claim://result', {
          claimId: args.claimId, accountId: args.id, accountName: summary.accountName,
          revision: args.revision, planId: args.planId, planName: summary.planName, ...extra }), 0);
        fixture.account5Starts = fixture.account5Starts || 0;
        if (args.id === 'account-0') {
          emit({ ok: false, code: 1005, message: 'synthetic rejection' });
        } else if (args.id === 'account-5' && fixture.account5Starts++ === 0) {
          // First attempt: no event; the test closes the captcha window instead.
        } else {
          emit({ ok: true, startsAt: null, endsAt: null });
        }
        return summary;
      }
      if (command === 'claim_status') {
        if (args.claimId === 'sub-3') {
          fixture.sub3Polls = (fixture.sub3Polls || 0) + 1;
          return fixture.session('sub-3', 'account-3', fixture.sub3Polls <= 2 ? 'submitting' : 'succeeded');
        }
        return null;
      }
      if (command === 'claim_cancel') return { phase: 'cancelled', cancelled: true, submissionInFlight: false };
      if (command === 'claim_acknowledge_unknown') { fixture.acknowledged.push(args.claimId); return true; }
      if (command === 'claim_sessions') {
        return { sessions: [fixture.session('old-2', 'account-2', 'unknown'), fixture.session('old-4', 'account-4', 'unknown'),
          fixture.session('sub-3', 'account-3', 'submitting')], recovery: { ok: true } };
      }
      if (command === 'set_behavior') return;
      throw new Error(`Unexpected synthetic command: ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "claim-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 900})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")
        expect(page.locator(".row[data-id]")).to_have_count(11)
        row = lambda i: page.locator(f'.row[data-id="account-{i}"]')

        # Recovered sessions: an unknown result and an in-flight submission are both visible.
        expect(row(2)).to_contain_text("领取结果待确认")
        expect(row(2)).to_contain_text("检查领取结果")
        expect(row(2)).not_to_contain_text("领取失败")
        expect(row(4)).to_contain_text("领取结果待确认")
        expect(row(3)).to_contain_text("已提交，正在确认结果")
        page.screenshot(path=str(output / "recovered.png"), full_page=True)
        # The recovered submission finishes, which releases the claim mutex.
        expect(row(3)).not_to_contain_text("已提交，正在确认结果", timeout=10000)

        # Batch eligibility refresh: per-item progress, a failure row, and no fixed 5s gap.
        refresh_button = page.locator(".toolbar button").filter(has_text="刷新资格").first
        started = time.monotonic()
        refresh_button.click()
        expect(page.locator(".toolbar")).to_contain_text(re.compile(r"刷新资格 \d+/11"), timeout=10000)
        expect(row(7)).to_contain_text("refresh_failed", timeout=10000)
        expect(row(7)).to_contain_text("重试本账号")
        elapsed = time.monotonic() - started
        assert elapsed < 5.0, f"batch refresh took {elapsed:.1f}s; the fixed per-account wait is still present"
        assert page.evaluate("claimFixture.calls.filter(c => c.command === 'claim_refresh').length") == 11
        assert page.evaluate("claimFixture.calls.some(c => c.command === 'claim_captcha_submit')") is False

        # A single-account retry refreshes only that account and keeps the batch cooldown.
        expect(refresh_button).to_be_disabled()
        page.evaluate("claimFixture.retriedAccount7 = true")
        row(7).get_by_role("button", name="重试本账号").click()
        expect(row(7)).not_to_contain_text("refresh_failed")
        assert page.evaluate("claimFixture.calls.filter(c => c.command === 'claim_refresh' && c.args.id === 'account-7').length") == 2
        expect(refresh_button).to_be_disabled()

        # Checking an unconfirmed claim reads quota and previews only; it never submits.
        starts_before = page.evaluate("claimFixture.startCount")
        row(2).get_by_role("button", name="检查领取结果").click()
        expect(page.locator(".toast-zone")).to_contain_text("已核对当前额度与可领套餐")
        assert page.evaluate("claimFixture.calls.some(c => c.command === 'claim_preview' && c.args.id === 'account-2')")
        assert page.evaluate("claimFixture.startCount") == starts_before
        page.screenshot(path=str(output / "unconfirmed.png"), full_page=True)

        # Explicit retry requires confirmation, acknowledges the unknown, then opens one new claim.
        row(2).get_by_role("button", name="已核对，重新尝试").click()
        expect(page.get_by_role("alertdialog")).to_contain_text("仅这一次会新建领取会话")
        page.get_by_role("button", name="仅重试这次").click()
        expect(row(2)).not_to_contain_text("领取结果待确认", timeout=10000)
        assert "old-2" in page.evaluate("claimFixture.acknowledged")
        new_ids = page.evaluate("claimFixture.claimIds")
        assert len(new_ids) == 1 and new_ids[0] != "old-2", new_ids

        # Closing the captcha window cancels the backend session; the row must not
        # wait for the frontend timeout when claim://cancelled arrives.
        row(5).get_by_role("button", name="领取").click()
        page.wait_for_function("claimFixture.claimIds.length === 2", timeout=10000)
        cancelled_id = page.evaluate("claimFixture.claimIds[1]")
        page.evaluate(f"window.__emitToPage('claim://cancelled', {{ claimId: '{cancelled_id}' }})")
        expect(row(5).get_by_role("button", name="领取")).to_be_enabled(timeout=5000)

        # Batch claim: a failed first account does not stop the next one.
        starts_before_batch = page.evaluate("claimFixture.startCount")
        page.locator(".toolbar .claim-all").click()
        page.wait_for_function(f"claimFixture.startCount >= {starts_before_batch + 2}", timeout=10000)
        ids = page.evaluate("claimFixture.calls.filter(c => c.command === 'claim_start').map(c => c.args.id)")
        assert ids[starts_before_batch:starts_before_batch + 2] == ["account-0", "account-1"], ids
        assert "account-4" not in ids[starts_before_batch:], "unconfirmed accounts never join a batch"
        expect(row(4)).to_contain_text("领取结果待确认")
        expect(page.locator(".toolbar .claim-all")).to_be_enabled(timeout=40000)
        assert "old-2" not in page.locator("#app").inner_text()
        assert "plan-1" not in page.locator("#app").inner_text()

        # English + 520px layout keeps buttons and retry rows readable.
        page.evaluate("claimFixture.language = 'en'; window.__emitToPage('state-changed', {})")
        expect(page.locator(".toolbar")).to_contain_text("Refresh", timeout=10000)
        expect(page.locator(".toolbar .claim-all")).to_contain_text("Claim all")
        page.set_viewport_size({"width": 520, "height": 820})
        page.screenshot(path=str(output / "narrow-en.png"), full_page=True)
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth"), "horizontal overflow"

        assert not errors, errors
        browser.close()
    print("PASS: 11 synthetic accounts, recovered sessions, batch progress, single retry without cooldown reset, "
          "unconfirmed check, explicit retry, batch continuation, English and 520px layout. No real API requests.")


if __name__ == "__main__":
    main()
