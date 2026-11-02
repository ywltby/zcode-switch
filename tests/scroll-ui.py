"""Exercise real wheel scrolling while mocked account updates render the UI."""
from playwright.sync_api import sync_playwright, expect

MOCK = r"""
(() => {
  const f = window.scrollFixture = {
    accounts: Array.from({length: 18}, (_, i) => ({id:`a${i}`, name:`Synthetic ${i}`,
      quota_revision:'r1', has_config:true, has_user_info:true,
      identity:{provider:'zai',username:`synthetic-${i}`}})),
    completed:0, extra:false, refreshes:0,
    state() { return {accounts:structuredClone(this.accounts),language:'zh',auto_claim:false,
      zcode_running:false,live_exists:false,live_logged_in:false,zcode_path_ok:true,hot_switch:false}; },
  };
  let callback=0;
  window.__TAURI_INTERNALS__ = {
    transformCallback() { return ++callback; },
    async invoke(command,args={}) {
      if (command === 'get_state') return f.state();
      if (command === 'app_version') return 'synthetic-scroll-test';
      if (command === 'get_account_quota') {
        await new Promise(r => setTimeout(r,15)); f.completed++;
        const items = f.extra && args.id === 'a0'
          ? Array.from({length:4}, (_,i) => ({name:`Synthetic quota ${i}`,total:100,used:20,remaining:80,percent_used:20,unit:'credits'})) : [];
        return {account_id:args.id,revision:args.revision,completion:'complete',errors:[],
          refreshed_at:1800000000000,is_empty:false,items:[],plans:[{name:'Synthetic plan',tier:'Pro',tier_code:'pro',items}]};
      }
      if (command === 'claim_preview') return {accountId:args.id,revision:args.revision,plans:[]};
      if (command === 'claim_sessions') return {sessions:[],recovery:{ok:true}};
      if (command.startsWith('plugin:event|') || command === 'reveal_main') return;
      throw new Error(`Unexpected command ${command}`);
    }
  };
})();
"""


def main():
    errors = []
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 680, "height": 680})
        page.on("pageerror", lambda e: errors.append(str(e)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda r: r.continue_() if r.request.url.startswith("http://127.0.0.1:5173") else r.abort())
        page.goto("http://127.0.0.1:5173")
        expect(page.locator('.row[data-id]')).to_have_count(18)
        page.wait_for_function('scrollFixture.completed >= 18', timeout=10000)
        expect(page.locator('#splash')).to_have_count(0)
        page.evaluate('window.initialList = document.querySelector(".list")')
        box = page.locator('.list').bounding_box()
        page.mouse.move(box['x'] + 160, box['y'] + 140)
        page.mouse.wheel(0, 350)
        page.wait_for_function('document.querySelector(".list").scrollTop > 200')
        before = page.locator('.list').evaluate('(el) => el.scrollTop')
        page.evaluate("""window.updateTimer = setInterval(() => {
          scrollFixture.refreshes++;
          scrollFixture.accounts.forEach((a,i) => { a.name=`Synthetic ${i} ${scrollFixture.refreshes % 2}`; });
          window.actions.refresh();
        }, 35)""")
        for _ in range(14):
            page.mouse.wheel(0, 100)
            page.wait_for_timeout(35)
        page.evaluate('clearInterval(window.updateTimer)')
        page.wait_for_timeout(120)
        result = page.evaluate("""() => ({sameList:window.initialList === document.querySelector('.list'),
          scroll:document.querySelector('.list').scrollTop, updates:scrollFixture.refreshes})""")
        print(f"wheel: before={before} after={result['scroll']} renders={result['updates']} same_list={result['sameList']}", flush=True)
        assert result['updates'] >= 5
        assert result['scroll'] >= before + 500, 'Wheel movement was lost during background rendering'
        assert result['sameList'], 'The list receiving the wheel gesture was replaced during refresh'

        # The same interaction works in the opposite direction at the minimum window width.
        page.set_viewport_size({"width": 520, "height": 680})
        box = page.locator('.list').bounding_box()
        page.mouse.move(box['x'] + 140, box['y'] + 140)
        before_up = page.locator('.list').evaluate('(el) => el.scrollTop')
        page.evaluate("""window.updateTimer = setInterval(() => {
          scrollFixture.refreshes++;
          scrollFixture.accounts.forEach((a,i) => { a.name=`Synthetic ${i} ${scrollFixture.refreshes % 2}`; });
          window.actions.refresh();
        }, 35)""")
        for _ in range(12):
            page.mouse.wheel(0, -100)
            page.wait_for_timeout(35)
        page.evaluate('clearInterval(window.updateTimer)')
        page.wait_for_timeout(120)
        after_up = page.locator('.list').evaluate('(el) => el.scrollTop')
        assert after_up <= before_up - 500, (before_up, after_up)
        assert page.evaluate('window.initialList === document.querySelector(".list")')
        assert page.evaluate('document.documentElement.scrollWidth <= window.innerWidth')

        # A quota result growing above the viewport keeps the visible row in place.
        page.evaluate("""() => {const list=document.querySelector('.list'); list.scrollTop=700;
          const row=[...list.querySelectorAll('.row[data-id]')].find(r=>r.getBoundingClientRect().bottom>list.getBoundingClientRect().top);
          window.anchor = {id:row.dataset.id, offset:row.getBoundingClientRect().top-list.getBoundingClientRect().top};
          scrollFixture.extra=true;}""")
        page.evaluate("window.actions.acctQuota('a0')")
        expect(page.locator('.row[data-id="a0"]')).to_contain_text('Synthetic quota 3')
        anchored = page.evaluate("""() => {const list=document.querySelector('.list'),
          row=list.querySelector(`[data-id="${window.anchor.id}"]`);
          return {id:row.dataset.id,offset:row.getBoundingClientRect().top-list.getBoundingClientRect().top,
            expected:window.anchor.offset,sameList:window.initialList === list};}""")
        assert anchored['sameList'] and abs(anchored['offset']-anchored['expected']) < 2, anchored

        # Removing the anchored account falls back to the previous scroll position.
        previous = page.locator('.list').evaluate('(el) => el.scrollTop')
        page.evaluate("""async () => { scrollFixture.accounts=scrollFixture.accounts.filter(a => a.id !== window.anchor.id);
          await window.actions.refresh(); }""")
        expect(page.locator('.row[data-id]')).to_have_count(17)
        assert abs(page.locator('.list').evaluate('(el) => el.scrollTop')-previous) < 2
        assert page.evaluate('window.initialList === document.querySelector(".list")')
        assert not errors, errors
        browser.close()
    print('PASS: wheel scrolling during refresh and visible-row position after quota changes; no real API requests.')


if __name__ == '__main__':
    main()
