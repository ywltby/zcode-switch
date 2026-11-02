"""Exercise automatic and manual CAPTCHA windows with a simulated SDK and IPC."""
from playwright.sync_api import sync_playwright


MOCK = r"""
(() => {
  const fixture = window.captchaFixture = {callbacks:null, events:[], submissions:[]};
  window.initAliyunCaptcha = callbacks => { fixture.callbacks = callbacks; };
  window.__TAURI_INTERNALS__ = {
    async invoke(command, args = {}) {
      if (command === 'get_state') return {language:'zh'};
      if (command === 'claim_status') return {status:'verifying'};
      if (command === 'claim_captcha_config') return {enabled:true, scene_id:'synthetic', region:'cn', prefix:'synthetic'};
      if (command === 'plugin:event|emit') { fixture.events.push(args); return; }
      if (command === 'claim_captcha_submit') { fixture.submissions.push(args); return {ok:true}; }
      throw new Error(`Unexpected simulated command ${command}`);
    },
  };
})();
"""


def main():
    errors = []
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch(headless=True)
        page = browser.new_page()
        page.on("pageerror", lambda error: errors.append(str(error)))
        page.add_init_script(MOCK)
        page.route("**/*", lambda route: route.continue_()
                   if route.request.url.startswith("http://127.0.0.1:5173") else route.abort())

        page.goto("http://127.0.0.1:5173/captcha.html?claimId=synthetic-auto&auto=1")
        page.wait_for_function("captchaFixture.callbacks !== null")
        page.evaluate("captchaFixture.callbacks.fail('interactive'); captchaFixture.callbacks.success('late')")
        page.wait_for_function("captchaFixture.events.some(e => e.event === 'captcha://interactive')")
        assert page.evaluate("captchaFixture.submissions.length") == 0

        page.goto("http://127.0.0.1:5173/captcha.html?claimId=synthetic-manual&auto=0")
        page.wait_for_function("captchaFixture.callbacks !== null")
        page.evaluate("captchaFixture.callbacks.fail('interactive'); captchaFixture.callbacks.success('manual')")
        page.wait_for_function("captchaFixture.submissions.length === 1")
        assert page.evaluate("captchaFixture.submissions[0].claimId") == "synthetic-manual"
        assert not errors, errors
        browser.close()
    print("PASS: auto interactive stops late submission; manual interactive can submit; no real API calls.")


if __name__ == "__main__":
    main()
