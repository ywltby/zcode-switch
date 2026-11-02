"""Offline UI coverage for the per-account eligibility button.

Run against the local Vite server on 127.0.0.1:5173. All accounts, responses,
and claim submissions are simulated; external network requests are blocked.
"""
from pathlib import Path

from playwright.sync_api import expect, sync_playwright


MOCK = r"""
(() => {
  const account = (id, name, provider) => ({
    id, name, quota_revision:'r1', has_config:true, has_user_info:true,
    identity:{provider, username:`user-${id}`},
  });
  const plan = {plan_id:'plan-1',name:'Synthetic grant',grants:[]};
  const f = window.singleClaimFixture = {
    accounts:[account('a0','示例 A','zai'), account('a1','示例 B','bigmodel'), account('a2','示例 C','zai')],
    language:'zh', calls:[], starts:[], sessions:{}, holdFirst:true, releaseFirst:null,
  };
  const callbacks = new Map(), listeners = new Map(); let callback = 0;
  window.singleClaimEmit = (event,payload) => callbacks.get(listeners.get(event))?.({event,payload});
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback,fn); return callback; },
    async invoke(command,args={}) {
      f.calls.push({command,args});
      if (command === 'plugin:event|listen') { listeners.set(args.event,args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-single-test';
      if (command === 'get_state') return {accounts:structuredClone(f.accounts),language:f.language,auto_claim:false,
        zcode_running:false,live_exists:false,live_logged_in:false,zcode_path_ok:true,hot_switch:false};
      if (command === 'get_account_quota') return {account_id:args.id,revision:args.revision,
        completion:'complete',refreshed_at:1800000000000,is_empty:true,items:[],plans:[],errors:[]};
      if (command === 'claim_refresh' || command === 'claim_preview') {
        if (args.id === 'a0' && f.holdFirst) {
          f.holdFirst=false;
          await new Promise(resolve => { f.releaseFirst=resolve; });
        }
        if (args.id === 'a2' && f.scopeChanged) throw JSON.stringify({code:'scope_changed',notStarted:false});
        if (args.id === 'a2') throw 'synthetic_scan_failure';
        return {accountId:args.id,revision:args.revision,plans:args.id === 'a0' ? [plan] : [],
          activated:true,activationError:null};
      }
      if (command === 'claim_sessions') return {sessions:[],recovery:{ok:true}};
      if (command === 'claim_start') {
        f.starts.push(args);
        const session={claimId:args.claimId,accountId:args.id,revision:args.revision,
          planId:args.planId,planName:plan.name,status:'submitting',auto:true};
        return f.sessions[args.claimId]=session;
      }
      if (command === 'claim_status') return f.sessions[args.claimId] || null;
      if (command === 'claim_cancel') return {phase:'submitting',cancelled:false,submissionInFlight:true};
      throw new Error(`Unexpected synthetic command ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / "src-tauri/target/claim-single-ui"
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 820})
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda route: route.continue_() if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())
        page.goto("http://127.0.0.1:5173")
        expect(page.locator('.row[data-id]')).to_have_count(3)
        row = lambda id: page.locator(f'.row[data-id="{id}"]')
        check = lambda id: row(id).get_by_role('button', name='查资格', exact=True)

        # Each button sits immediately before its own quota button.
        for id in ('a0', 'a1', 'a2'):
            expect(check(id)).to_be_enabled()
            assert check(id).evaluate("el => el.nextElementSibling?.getAttribute('aria-label') === '查额度'")
        expect(page.get_by_role('switch', name='自动领取')).to_have_attribute('aria-checked', 'false')

        # An in-flight single-account query is visible, deduplicated, and does not scan other accounts.
        check('a0').click()
        page.wait_for_function("singleClaimFixture.releaseFirst !== null")
        expect(check('a0')).to_be_disabled()
        expect(check('a0')).to_have_attribute('aria-busy', 'true')
        page.evaluate("actions.refreshClaimOne('a0')")
        assert page.evaluate("singleClaimFixture.calls.filter(c => c.command === 'claim_refresh').map(c => c.args.id)") == ['a0']

        # A different account can be checked while the first query is waiting.
        check('a1').click()
        expect(page.locator('.toast-zone')).to_contain_text('「示例 B」暂无可领取的套餐')
        assert page.evaluate("singleClaimFixture.calls.filter(c => c.command === 'claim_refresh').map(c => c.args.id)") == ['a0', 'a1']
        expect(check('a0')).to_be_disabled()

        # A successful result enters the same background queue, even with auto scanning off.
        page.evaluate('singleClaimFixture.releaseFirst()')
        page.wait_for_function('singleClaimFixture.starts.length === 1')
        first = page.evaluate('singleClaimFixture.starts[0]')
        assert first['id'] == 'a0' and first['mode'] == 'auto', first
        expect(row('a0')).to_contain_text('领取中')
        expect(check('a0')).to_be_enabled()

        # Eligibility checks remain usable during a held claim; failures stay visible and do not submit.
        check('a2').click()
        expect(row('a2')).to_contain_text('synthetic_scan_failure')
        expect(row('a2')).to_contain_text('重试本账号')
        assert page.evaluate('singleClaimFixture.starts.length') == 1
        page.evaluate('singleClaimFixture.scopeChanged=true')
        check('a2').click()
        expect(page.locator('.toast-zone')).to_contain_text('「示例 C」账号状态已变化，请重新查询资格')
        expect(check('a2')).to_be_enabled()
        check('a0').click()
        page.wait_for_function("singleClaimFixture.calls.filter(c => c.command === 'claim_refresh' && c.args.id === 'a0').length === 2")
        assert page.evaluate('singleClaimFixture.starts.length') == 1, 'a repeated query must not duplicate the active claim'

        # English and the minimum window width retain readable, non-overlapping controls.
        page.evaluate("singleClaimFixture.language='en'; singleClaimEmit('state-changed',{})")
        page.set_viewport_size({"width": 520, "height": 820})
        english = row('a1').get_by_role('button', name='Check eligibility', exact=True)
        expect(english).to_be_enabled()
        assert english.evaluate("el => el.nextElementSibling?.getAttribute('aria-label') === 'Quota'")
        assert english.evaluate("""el => {
          const button=el.getBoundingClientRect(), row=el.closest('.row').getBoundingClientRect();
          return button.left>=row.left && button.right<=row.right;
        }""")
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
        page.screenshot(path=str(output / 'single-eligibility-520.png'), full_page=True)
        assert not errors, errors
        browser.close()
    print('PASS: per-account query, busy state, empty/error feedback, background queue, deduplication and 520px English layout; no real API calls.')


if __name__ == '__main__':
    main()
