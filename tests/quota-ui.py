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
    hold: {}, pending: [], failures: {}, overrides: {}, trace: [],
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
        return {...fixture.quota(args.id,args.revision), ...fixture.overrides[args.id]};
      }
      if (command === 'update_account_from_live') {
        const a = fixture.accounts.find(a => a.id === args.id); a.quota_revision = 'r2'; return a;
      }
      if (command === 'delete_account') { fixture.accounts = fixture.accounts.filter(a => a.id !== args.id); return; }
      if (command === 'switch_to') {
        fixture.accounts.forEach(a => a.is_active = a.id === args.id);
        return { switched: true, name: args.id, probe: null };
      }
      if (command === 'claim_preview') return { accountId: args.id, revision: args.revision, plans: [] };
      if (command === 'claim_sessions') return { sessions: [], recovery: { ok: true } };
      if (command === 'claim_legacy_count') return [];
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
        page = browser.new_page(viewport={"width": 680, "height": 820}, timezone_id="Asia/Shanghai")
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())
        page.goto("http://127.0.0.1:5173")
        page.wait_for_load_state("networkidle")
        expect(page.locator(".row[data-id]")).to_have_count(3)
        row = lambda account: page.locator(f'.row[data-id="account-{account}"]')
        quota = lambda account: row(account).locator(f'''button[click="actions.acctQuota('account-{account}')"]''').click()
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        expect(row("a")).not_to_contain_text("更新于")

        # Channel badges belong beside the account name, including during rename.
        expect(row("a").locator(".row-name .channel-badge")).to_have_text("z.ai · 国际")
        expect(row("b").locator(".row-name .channel-badge")).to_have_text("bigmodel.cn · 国内")
        expect(page.locator(".row-meta .channel-badge")).to_have_count(0)
        page.evaluate("actions.rename('account-a')")
        expect(row("a").locator(".row-name .rename-input")).to_have_count(1)
        expect(row("a").locator(".row-name .channel-badge")).to_have_text("z.ai · 国际")
        page.evaluate("actions.cancelRename()")

        # At the minimum window width, a long name yields space to its badge.
        narrow = browser.new_page(viewport={"width": 520, "height": 760}, timezone_id="Asia/Shanghai")
        narrow.add_init_script(MOCK + """
          quotaFixture.accounts[0].name='国际预览账号';
          quotaFixture.accounts[1].name='国内预览账号';
          quotaFixture.accounts[2].name='Long account name '.repeat(3);
        """)
        narrow.route("**/*", lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())
        narrow.goto("http://127.0.0.1:5173")
        for account, label in (("a", "z.ai · 国际"), ("b", "bigmodel.cn · 国内"), ("c", "z.ai · 国际")):
            line = narrow.locator(f'.row[data-id="account-{account}"] .row-name')
            badge = line.locator('.channel-badge')
            expect(badge).to_have_text(label)
            expect(line.locator('.tier-b')).to_have_count(1)
            assert line.evaluate("""el => {
              const name=el.querySelector('.row-name-text').getBoundingClientRect();
              const badge=el.querySelector('.channel-badge').getBoundingClientRect();
              const line=el.getBoundingClientRect();
              return badge.left>=line.left && badge.right<=line.right && Math.abs(name.top-badge.top)<7;
            }""")
            if account != "c":
                assert line.locator('.row-name-text').evaluate("el => el.scrollWidth <= el.clientWidth"), account
                assert line.evaluate("""el => {
                  const name=el.querySelector('.row-name-text').getBoundingClientRect();
                  const tier=el.querySelector('.tier-b').getBoundingClientRect();
                  return Math.abs(name.top-tier.top)<7;
                }"""), account
        assert narrow.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        narrow.close()

        quota("b")
        expect(row("b")).to_contain_text("部分额度查询未完成")
        expect(row("b")).to_contain_text("暂时无法确认 Coding Plan 额度")
        expect(row("b")).not_to_contain_text("更新于")
        quota("c")
        expect(row("c")).to_contain_text("Synthetic Coding Pro")
        page.screenshot(path=str(output / "partial.png"), full_page=True)

        # A failed refresh retains the last result and its timestamp in that row.
        page.evaluate("quotaFixture.failures['account-a'] = 'network_error'")
        quota("a")
        expect(row("a")).to_contain_text("刷新失败，显示")
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        expect(row("b")).to_contain_text("暂时无法确认 Coding Plan 额度")

        # An empty partial response cannot erase cached quota or claim a new success time.
        previous_status = row("a").locator(".aq-status").last.inner_text()
        page.evaluate("""delete quotaFixture.failures['account-a'];
          quotaFixture.overrides['account-a'] = {
            completion: 'partial', plans: [], items: [], is_empty: false,
            errors: [{scope: 'ZCode', code: 'network_error'}], refreshed_at: 1800000300000
          }""")
        quota("a")
        expect(row("a")).to_contain_text("Synthetic Coding Pro")
        expect(row("a")).to_contain_text("刷新失败，显示")
        expect(row("a").locator(".aq-status").last).to_have_text(previous_status)
        page.evaluate("delete quotaFixture.overrides['account-a']")

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

        # Every known start is visible; future plans are marked pending at 520px.
        page.evaluate("""quotaFixture.overrides['account-a'] = {plans: [
          {name:'Future '.repeat(15), tier:'Start', tier_code:'start', starts_at:2000000000000, ends_at:2002678400000,
            expire:'2033-06-18 11:33', items:[]},
          {name:'Already active', tier:'Pro', tier_code:'pro', starts_at:1600000000000, items:[]},
          {name:'Still waiting', tier:'Pro', tier_code:'pro', pending:true,
            starts_at:1600000000000, ends_at:2002678400000, items:[]},
          {name:'Waiting with no time', tier:'Pro', tier_code:'pro', pending:true, items:[]},
          {name:'Start unknown', tier:'Pro', tier_code:'pro', items:[]},
          {name:'Expired exact', tier:'Max', tier_code:'max', starts_at:1590000000000,
            ends_at:1600000000000, expire:'2020-09-13 20:26', items:[]},
          {name:'Expired legacy', tier:'Max', tier_code:'max', expire:'2020-01-01', items:[]}
        ], plan_expire:'2020-01-01'}""")
        quota("a")
        expect(row("a").locator(".pg-start")).to_have_count(4)
        expect(row("a").locator(".pg-start")).to_have_text([
            "待生效 · 2033-05-18 11:33 生效", "生效时间：2020-09-13 20:26",
            "待生效 · 生效时间：2020-09-13 20:26", "待生效"
        ])
        expect(row("a")).not_to_contain_text("Expired exact")
        expect(row("a")).not_to_contain_text("Expired legacy")
        expect(row("a")).not_to_contain_text("有效期至 2020-01-01")
        expect(row("a").locator(".row-name .tier-b.max")).to_have_count(0)
        expect(row("a")).to_contain_text("至 2033-06-18")
        same_line = row("a").locator('.plan-grp').first.evaluate("""group => {
          const start=group.querySelector('.pg-start'), end=group.querySelector('.pg-exp');
          const a=start.getBoundingClientRect(), b=end.getBoundingClientRect();
          return start.parentElement === end.parentElement && Math.abs(a.top-b.top)<4 && a.right<=b.left;
        }""")
        assert same_line, 'Activation time and expiration should share a row at 520px'
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth"), "future plan overflow"
        page.screenshot(path=str(output / "future-plan-narrow.png"), full_page=True)
        page.evaluate("window.realNow = Date.now; Date.now = () => 2000000000000")
        expect(row("a").locator(".pg-start")).to_have_text([
            "生效时间：2033-05-18 11:33", "生效时间：2020-09-13 20:26",
            "待生效 · 生效时间：2020-09-13 20:26", "待生效"
        ], timeout=7000)
        page.evaluate("Date.now = () => 2002678400000")
        expect(row("a")).not_to_contain_text("Future Future", timeout=7000)
        expect(row("a")).to_contain_text("Already active")
        page.evaluate("Date.now = window.realNow")
        page.evaluate("quotaFixture.overrides['account-a'].plans[0].starts_at = Date.now() - 60000")
        quota("a")
        expect(row("a").locator(".pg-start")).to_have_count(4)
        expect(row("a").locator(".pg-start")).to_contain_text([
            "生效时间：", "生效时间：", "待生效", "待生效"
        ])

        # Reproduce the observed local case: claimed at 19:09, effective at 23:00.
        page.evaluate("""window.realNow = Date.now; Date.now = () => 1793111645000;
          quotaFixture.overrides['account-a'] = {plans: [{
            name:'ZCODE GLOBAL BUILD',tier:'Start',tier_code:'start',scope_label:'Start Plan',
            starts_at:1793099353000,ends_at:1793149200000,expire:'2026-10-28 09:00',
            entitlement_starts:[{name:'GLM-5.3-Flash',effective_at:1793113200000}],items:[]
          }]}""")
        quota("a")
        expect(row("a").locator(".pg-start")).to_have_text("待生效 · 2026-10-27 23:00 生效")
        expect(row("a").locator(".pg-start")).to_have_attribute("title", "GLM-5.3-Flash · 待生效 · 2026-10-27 23:00 生效")
        expect(row("a").locator(".pg-exp")).to_have_text("至 2026-10-28 09:00")
        same_line = row("a").locator('.plan-grp').first.evaluate("""group => {
          const start=group.querySelector('.pg-start'), end=group.querySelector('.pg-exp');
          const a=start.getBoundingClientRect(), b=end.getBoundingClientRect();
          return start.parentElement === end.parentElement && Math.abs(a.top-b.top)<4 && a.right<=b.left;
        }""")
        assert same_line and page.evaluate("document.documentElement.scrollWidth <= window.innerWidth")
        typography = row("a").locator(".pg-head").evaluate("""head => {
          const metrics = selector => {
            const el = head.querySelector(selector), css = getComputedStyle(el);
            const marker = document.createElement('span');
            marker.style.cssText = 'display:inline-block;width:0;height:0;vertical-align:baseline';
            el.appendChild(marker);
            const baseline = marker.getBoundingClientRect().top;
            marker.remove();
            return {font:css.fontFamily, size:css.fontSize, weight:css.fontWeight,
              lineHeight:css.lineHeight, baseline};
          };
          return {start:metrics('.pg-start'), end:metrics('.pg-exp')};
        }""")
        for field in ("font", "size", "weight", "lineHeight"):
            assert typography["start"][field] == typography["end"][field], (field, typography)
        assert abs(typography["start"]["baseline"] - typography["end"]["baseline"]) <= 0.5, typography
        assert row("a").locator(".pg-scope").evaluate("""el =>
          el.getBoundingClientRect().height <= parseFloat(getComputedStyle(el).fontSize) * 1.6
        """), "the scope label must stay on one line at 520px"
        page.screenshot(path=str(output / "activation-same-line.png"), full_page=True)
        expect(row("a")).not_to_contain_text("2026-10-27 19:09")
        page.evaluate("Date.now = window.realNow")

        # Start Plan entitlements have their own effective_at, distinct from claim time.
        page.evaluate("""quotaFixture.overrides['account-a'] = {plans: [
          {name:'ZCODE GLOBAL BUILD',tier:'Start',tier_code:'start',starts_at:1600000000000,
           entitlement_starts:[
             {name:'GLM-5.3-Flash',effective_at:2000000000000},
             {name:'Other model',effective_at:2000003600000},
             {name:'Unknown time',effective_at:null}
           ],items:[]},
          {name:'Start with no effective time',tier:'Start',tier_code:'start',
           starts_at:1600000000000,entitlement_starts:[],items:[]},
          {name:'Coding plan',tier:'Pro',tier_code:'pro',starts_at:1600000000000,items:[]}
        ]}""")
        quota("a")
        expect(row("a").locator(".pg-start")).to_have_text([
          "待生效 · 2033-05-18 11:33 生效", "生效时间未知", "生效时间：2020-09-13 20:26"
        ])
        expect(row("a").locator(".pg-start-more")).to_have_text([
          "GLM-5.3-Flash · 待生效 · 2033-05-18 11:33 生效",
          "Other model · 待生效 · 2033-05-18 12:33 生效", "Unknown time · 生效时间未知"
        ])
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth"), "activation time overflow"

        # CREDIT_LIMIT also has timed windows; show their labels and reset times.
        page.evaluate("""quotaFixture.overrides['account-a'] = {plans: [{
          name: 'Synthetic very long Coding Plan name '.repeat(5), tier: 'Pro', tier_code: 'pro',
          items: [
            {kind: 'raw', name: 'CREDIT_LIMIT（每 5 小时）', window: 'hours:5',
             total: 2000, remaining: 1552, percent_used: 22.35, reset: '10-26 19:31'},
            {kind: 'raw', name: 'CREDIT_LIMIT（每周）', window: 'weekly',
             total: 2000, remaining: 5, percent_used: 99.75, reset: '11-02 09:24'}
          ]
        }]}""")
        quota("a")
        expect(row("a").locator(".q-win-label")).to_have_text(["每 5 小时", "每周"])
        expect(row("a").locator(".q-win-reset")).to_have_text(["10-26 19:31 重置", "11-02 09:24 重置"])
        expect(row("a").locator(".qbar-pct")).to_have_text(["78%", "<1%"])
        page.screenshot(path=str(output / "credit-windows-narrow.png"), full_page=True)
        assert page.evaluate("document.documentElement.scrollWidth <= window.innerWidth"), "credit window overflow"

        # Almost-full, exactly-full, exhausted, and unknown quota stay distinguishable.
        for used, label in [(0.25, ">99%"), (0, "100%"), (100, "0%"), (None, "--")]:
            page.evaluate("""used => {
              quotaFixture.overrides['account-a'].plans[0].items = [
                {kind: 'raw', name: 'Synthetic credits', percent_used: used}
              ];
            }""", used)
            quota("a")
            expect(row("a").locator(".qbar-pct")).to_have_text(label)

        # An expired cached plan and its old quota never appear as current.
        page.evaluate("""quotaFixture.overrides['account-c'] = {
          completion:'complete', errors:[], is_empty:false,
          plans:[{name:'Expired only', tier:'Max', tier_code:'max', ends_at:1600000000000,
            expire:'2020-09-13 20:26', items:[{name:'Expired quota',total:100,used:0}]}],
          items:[{name:'Expired quota',total:100,used:0}], refreshed_at:1800000600000
        }""")
        quota("c")
        expect(row("c")).not_to_contain_text("Expired only")
        expect(row("c")).not_to_contain_text("Expired quota")
        expect(row("c")).to_contain_text("暂无有效套餐")

        # A confirmed empty result must still replace the previous plan after recovery.
        page.evaluate("""quotaFixture.overrides['account-c'] = {
          completion: 'complete', errors: [], plans: [], items: [], is_empty: true,
          refreshed_at: 1800000600000
        }""")
        quota("c")
        expect(row("c")).to_contain_text("未开通套餐")
        expect(row("c")).not_to_contain_text("Synthetic Coding Pro")
        expect(row("c")).not_to_contain_text("刷新失败，显示")

        assert not any("claim" in command and command not in ("claim_preview", "claim_sessions", "claim_legacy_count") for command in page.evaluate("quotaFixture.trace"))
        assert not errors, errors
        browser.close()
    print("PASS: synthetic A/B/C refresh, partial status, stale timestamp, credential update, state polling, late success/error, deletion, switching, and narrow layout. No real API requests.")


if __name__ == "__main__":
    main()
