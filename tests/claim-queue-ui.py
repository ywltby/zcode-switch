"""Offline acceptance: eligibility scans keep moving while one claim is held."""
from pathlib import Path
from playwright.sync_api import sync_playwright, expect

MOCK = r"""
(() => {
  const f = window.queueFixture = {
    accounts: Array.from({length: 11}, (_, i) => ({id: `a${i}`, name: `Synthetic ${i}`, quota_revision: 'r1',
      has_config: true, has_user_info: true, identity: {provider: 'zai', username: `synthetic-${i}`}})),
    auto: false, calls: [], refreshStarted: 0, refreshFinished: 0, queries: 0, peakQueries: 0,
    starts: [], sessions: {}, claimed: [], failSeven: true, claimActive: 0, peakClaims: 0,
    session(claimId, id, status) { return {claimId, accountId:id, accountName:id, revision:'r1', planId:'p', planName:'Synthetic grant', status, auto:true}; },
    complete(claimId) {
      const s = this.sessions[claimId]; if (!s || s.status === 'succeeded') throw new Error('No pending claim');
      s.status = 'succeeded'; s.result = {ok:true}; this.claimActive--; this.claimed.push(s.accountId);
      window.emitQueue('claim://result', {...s, ok:true});
    },
  };
  const callbacks = new Map(), listeners = new Map(); let callback = 0;
  window.emitQueue = (event, payload) => callbacks.get(listeners.get(event))?.({event,payload});
  window.__TAURI_INTERNALS__ = {
    transformCallback(fn) { callbacks.set(++callback,fn); return callback; },
    async invoke(command, args={}) {
      f.calls.push({command,args});
      if (command === 'plugin:event|listen') { listeners.set(args.event,args.handler); return 1; }
      if (command === 'plugin:event|unlisten' || command === 'reveal_main') return;
      if (command === 'app_version') return 'synthetic-queue-test';
      if (command === 'get_state') return {accounts:structuredClone(f.accounts),language:'zh',auto_claim:f.auto,
        zcode_running:false,live_exists:false,live_logged_in:false,zcode_path_ok:true,hot_switch:false};
      if (command === 'set_behavior') { f.auto=args.autoClaim; return; }
      if (command === 'get_account_quota') return {account_id:args.id,revision:args.revision,completion:'complete',
        refreshed_at:1800000000000,is_empty:true,items:[],plans:[],errors:[]};
      if (command === 'claim_refresh' || command === 'claim_preview') {
        if (command === 'claim_refresh') f.refreshStarted++;
        f.queries++; f.peakQueries=Math.max(f.peakQueries,f.queries);
        await new Promise(r=>setTimeout(r,args.id === 'a0' ? 30 : 250));
        f.queries--;
        if (command === 'claim_refresh') f.refreshFinished++;
        if (args.id === 'a7' && f.failSeven) throw 'synthetic_scan_failure';
        return {accountId:args.id,revision:args.revision,plans:f.claimed.includes(args.id)?[]:[{plan_id:'p',name:'Synthetic grant',grants:[]}],activated:true};
      }
      if (command === 'claim_start') {
        f.claimActive++; f.peakClaims=Math.max(f.peakClaims,f.claimActive);
        f.starts.push({...args,scansFinished:f.refreshFinished});
        return f.sessions[args.claimId]=f.session(args.claimId,args.id,'submitting');
      }
      if (command === 'claim_status') return f.sessions[args.claimId] || null;
      if (command === 'claim_sessions') return {sessions:[f.session('unknown-a4','a4','unknown')],recovery:{ok:true}};
      if (command === 'claim_cancel') return {phase:'submitting',cancelled:false,submissionInFlight:true};
      throw new Error(`Unexpected synthetic command ${command}`);
    },
  };
})();
"""


def main():
    output = Path(__file__).resolve().parents[1] / 'src-tauri/target/claim-queue-ui'
    output.mkdir(parents=True, exist_ok=True)
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={'width':680,'height':900})
        page.on('pageerror', lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route('**/*',lambda route: route.continue_() if route.request.url.startswith('http://127.0.0.1:5173') else route.abort())
        page.goto('http://127.0.0.1:5173')
        expect(page.locator('.row[data-id]')).to_have_count(11)
        expect(page.locator('.row[data-id="a4"]')).to_contain_text('领取结果待确认')
        switch = page.get_by_role('switch',name='自动领取')
        expect(switch).to_have_attribute('aria-checked','false')
        page.locator('.toolbar button').filter(has_text='刷新资格').first.click()
        page.wait_for_function('queueFixture.starts.length === 1', timeout=6000)
        first = page.evaluate('queueFixture.starts[0]')
        assert first['scansFinished'] < 11, first
        assert first['mode'] == 'auto'
        page.wait_for_function('queueFixture.refreshFinished === 11', timeout=6000)
        assert page.evaluate('queueFixture.starts.length') == 1
        assert page.evaluate('queueFixture.peakQueries') == 3
        expect(page.locator('.row[data-id="a1"]')).to_contain_text('排队中')

        # A single-account manual retry works while the first submission is held.
        page.evaluate('queueFixture.failSeven = false')
        page.locator('.row[data-id="a7"]').get_by_role('button',name='重试本账号').click()
        page.wait_for_function('queueFixture.refreshFinished === 12')
        expect(page.locator('.row[data-id="a7"]')).not_to_contain_text('synthetic_scan_failure')

        # The periodic scan can start during claiming, and duplicate results do not submit again.
        switch.click()
        expect(switch).to_have_attribute('aria-checked','true')
        page.wait_for_function('queueFixture.refreshFinished === 23', timeout=6000)
        assert page.evaluate('queueFixture.starts.length') == 1
        switch.click()
        expect(switch).to_have_attribute('aria-checked','false')

        # Removal, delete/re-add, and credential replacement invalidate queued snapshots.
        page.evaluate("""queueFixture.accounts=queueFixture.accounts.filter(a=>a.id!=='a1');
          queueFixture.accounts.find(a=>a.id==='a2').quota_revision='r2';
          window.emitQueue('state-changed',{});""")
        expect(page.locator('.row[data-id]')).to_have_count(10)
        page.evaluate("""queueFixture.accounts.push({id:'a1',name:'Re-added',quota_revision:'r1',has_user_info:true,identity:{provider:'zai'}});
          window.emitQueue('state-changed',{});""")
        expect(page.locator('.row[data-id]')).to_have_count(11)
        page.evaluate('queueFixture.complete(queueFixture.starts[0].claimId)')
        page.wait_for_function('queueFixture.starts.length === 2', timeout=10000)
        starts = page.evaluate('queueFixture.starts')
        assert starts[1]['id'] == 'a3', starts
        assert page.evaluate('queueFixture.peakClaims') == 1
        assert page.evaluate('queueFixture.auto') is False, 'queued work continues independently of periodic scans'
        assert all(x['id'] not in ['a1','a2','a4'] for x in starts)
        page.set_viewport_size({'width':520,'height':820})
        page.screenshot(path=str(output/'scanning-and-queue.png'),full_page=True)
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')
        assert not errors, errors
        browser.close()
    print('PASS: manual and timed scans enqueue immediately, scans continue during a held claim, single-account retry, serial delivery, dedupe, stale-account cancellation, unknown protection and 520px layout.')


if __name__ == '__main__':
    main()
