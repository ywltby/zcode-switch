"""Offline UI acceptance using synthetic accounts and a mocked Tauri boundary.

Run with the webapp-testing with_server.py helper, serving Vite on port 5173.
Requires Python Playwright + Chromium. No real credentials or official APIs.
"""
from pathlib import Path
from playwright.sync_api import sync_playwright, expect

MOCK = r"""
(() => {
  const makeAccount = (id, provider) => ({
    id, name: `Synthetic ${id.slice(-1).toUpperCase()}`, quota_revision: 'r1',
    is_active: id === 'account-a', has_config: true, has_user_info: true,
    identity: { provider, username: `synthetic-${id.slice(-1)}` },
  });
  const fixture = window.quotaFixture = {
    accounts: [makeAccount('account-a', 'zai'), makeAccount('account-b', 'bigmodel'), makeAccount('account-c', 'zai')],
    hold: {}, pending: [], failures: {}, trace: [],
    quota(id, revision, name = 'Synthetic Coding Pro') {
      return { account_id: id, revision, completion: id === 'account-b' ? 'partial' : 'complete',
        errors: id === 'account-b' ? [{scope: 'Coding Plan', code: 'project_key_missing'}] : [],
        refreshed_at: 1800000000000, is_empty: false, items: [], plans: [
          {name, tier: 'Pro', tier_code: 'pro', scope_label: 'Synthetic project', items: [
            {name: 'Synthetic credits', total: 100, used: 20, remaining: 80, percent_used: 20, unit: 'credits'}
          ]}
        ] };
    },
    state() { return {accounts: structuredClone(this.accounts), language: 'zh', zcode_running: false,
      live_exists: true, live_logged_in: true, zcode_path_ok: true, auto_claim: false, hot_switch: false}; },
    release(name = 'LATE OLD RESULT', error = null) {
      const pending = this.pending.shift();
      if (!pending) throw new Error('No synthetic pending request');
      error ? pending.reject(error) : pending.resolve(this.quota(pending.id, pending.revision, name));
    },
  };
  let callback = 0;
  window.__TAURI_INTERNALS__ = {
    transformCallback() { return ++callback; },
    async invoke(command, args = {}) {
      fixture.trace.push(command);
      if (command === 'get_state') return fixture.state();
      if (command === 'app_version') return 'synthetic-test';
      if (command === 'get_account_quota') {
        if (fixture.hold[args.id]) return new Promise((resolve,reject) => fixture.pending.push({...args,resolve,reject}));
        if (fixture.failures[args.id]) throw fixture.failures[args.id];
        return fixture.quota(args.id,args.revision);
      }
      if (command === 'update_account_from_live') {
        const a = fixture.accounts.find(a => a.id === args.id); a.quota_revision = 'r2'; return a;
      }
      if (command === 'delete_account') { fixture.accounts = fixture.accounts.filter(a => a.id !== args.id); return; }
      if (command === 'switch_to') {
        fixture.accounts.forEach(a => a.is_active = a.id === args.id);
        return { switched: true, name: args.id, probe: null };
      }
      if (command === 'claim_preview') return [];
      if (command.startsWith('plugin:event|')) return 1;
      if (['reveal_main','dismiss_splash'].includes(command)) return;
      throw new Error(`Unexpected synthetic command: ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri" / "target" / "quota-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 820})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")
        expect(page.locator(".row[data-id]")).to_have_count(3)
        row = lambda account: page.locator(f'.row[data-id="account-{account}"]')
        quota = lambda account: row(account).locator(f'''button[click="actions.acctQuota('account-{account}')"]''').click()
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        quota("b")
        expect(row("b")).to_contain_text("部分额度查询未完成")
        expect(row("b")).to_contain_text("暂时无法确认 Coding Plan 额度")
        quota("c")
        expect(row("c")).to_contain_text("Synthetic Coding Pro")
        page.screenshot(path=str(output / "partial.png"), full_page=True)

        # A failed refresh retains the last result and its timestamp in that row.
        page.evaluate("quotaFixture.failures['account-a'] = 'network_error'")
        quota("a")
        expect(row("a")).to_contain_text("刷新失败，显示")
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        expect(row("b")).to_contain_text("暂时无法确认 Coding Plan 额度")

        # Updating credentials while a request is pending discards the old success.
        page.evaluate("delete quotaFixture.failures['account-a']; quotaFixture.hold['account-a'] = true")
        quota("a")
        page.wait_for_function("quotaFixture.pending.length === 1")
        page.evaluate("actions.updateFromLive('account-a')")
        expect(row("a")).not_to_contain_text("Synthetic Coding Pro")
        page.evaluate("quotaFixture.hold['account-a'] = false")
        quota("a")
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        page.evaluate("quotaFixture.release('LATE OLD RESULT')")
        expect(row("a")).not_to_contain_text("LATE OLD RESULT")

        # The actual five-second state poll clears another changed login.
        page.evaluate("quotaFixture.hold['account-c'] = true")
        quota("c")
        page.wait_for_function("quotaFixture.pending.length === 1")
        page.evaluate("quotaFixture.accounts.find(a => a.id === 'account-c').quota_revision = 'r2'")
        expect(row("c")).not_to_contain_text("Synthetic Coding Pro", timeout=7000)
        page.evaluate("quotaFixture.hold['account-c'] = false; quotaFixture.release('', 'LATE OLD FAILURE')")
        expect(row("c")).not_to_contain_text("LATE OLD FAILURE")

        # Deleting a row discards its pending response without touching A or C.
        page.evaluate("quotaFixture.hold['account-b'] = true")
        quota("b")
        page.wait_for_function("quotaFixture.pending.length === 1")
        row("b").locator('''button[click="actions.delete('account-b')"]''').click()
        page.locator(".cm-yes").click()
        expect(row("b")).to_have_count(0)
        page.evaluate("quotaFixture.release('DELETED ACCOUNT RESULT')")
        expect(page.locator("#app")).not_to_contain_text("DELETED ACCOUNT RESULT")

        # Switching changes the active row while preserving account-specific quota.
        row("c").locator('''button[click="actions.askSwitch('account-c')"]''').click()
        expect(row("c")).to_have_class("row active")
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        quota("c")
        expect(row("c")).to_contain_text("Synthetic Coding Pro")
        page.set_viewport_size({"width": 520, "height": 760})
        page.screenshot(path=str(output / "final.png"), full_page=True)
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth"), "horizontal overflow"
        assert not any("claim" in command and command != "claim_preview" for command in page.evaluate("quotaFixture.trace"))
        assert not errors, errors
        browser.close()
    print("PASS: synthetic A/B/C refresh, partial status, stale timestamp, credential update, state polling, late success/error, deletion, switching, and narrow layout. No real API requests.")


if __name__ == "__main__":
    main()
