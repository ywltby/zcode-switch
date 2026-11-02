
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { init, t, lang, stripErr } from "./i18n.js";
import { createCaptchaFlow } from "./captcha-flow.js";

const SDK_URL = "https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js";
const query = new URLSearchParams(location.search);
const claimId = query.get("claimId") || "";
const auto = query.get("auto") === "1";

const $text = document.getElementById("cap-text");
const $detail = document.getElementById("cap-detail");
const $dot = document.getElementById("cap-dot");
const $btn = document.getElementById("cap-btn");

function status(text, tone = "run") {
  $text.textContent = text;
  $dot.className = "cap-dot" + (tone === "ok" ? " ok" : tone === "err" ? " err" : "");
}

function detail(text) {
  $detail.textContent = text || "";
}

function notify(event, payload) {
  emit(event, payload).catch(() => {});
}

document.addEventListener("securitypolicyviolation", (e) => {
  detail(t("c.cspBlocked", { directive: e.violatedDirective, uri: String(e.blockedURI).slice(0, 70) }));
});

function loadSdk() {
  return new Promise((resolve, reject) => {
    if (typeof window.initAliyunCaptcha === "function") return resolve();
    const script = document.createElement("script");
    script.src = SDK_URL;
    script.onload = () => resolve();
    script.onerror = () => reject(new Error(t("c.sdkFail")));
    document.head.appendChild(script);
  });
}

let submitting = false;
let region = null;
let tracelessTimer = 0;
let captchaInstance = null;

const labels = {
  preparing: () => t("c.preparing"),
  verifying: () => t("c.traceless"),
  interactive: () => t("c.interactive"),
  submitted: () => t("c.passed"),
  succeeded: () => t("c.succeeded"),
  failed: () => t("c.failed"),
  unknown: () => t("c.unknown"),
  already: () => t("c.already"),
  setup_failed: () => t("c.setupFailed"),
  submit_failed: () => t("c.submitFailed"),
};

  let cfg;
  try {
    cfg = await invoke("claim_captcha_config");
  } catch (e) {
    notifyStuck();
    status(t("c.cfgFail"), "err");
    detail(stripErr(e));
    return;
  }
  if (!cfg.enabled || !cfg.scene_id) {
    notifyStuck();
    status(t("c.cfgUnavailable"), "err");
    detail(t("c.cfgUnavailableDetail"));
    return;
  }
  region = cfg.region || null;
  try {
    await loadSdk();
  } catch (e) {
    notifyStuck();
    status(e.message || t("c.sdkFail"), "err");
    return;
  }

  window.AliyunCaptchaConfig = { region: cfg.region, prefix: cfg.prefix };

  status(t("c.traceless"));

  // 3007（验证码校验失败：参数过期/已用）→ 后端放回 pending 并返回 retry=true，
  // 这里重建验证码实例拿新参数自动重试一次（后端保证只放回一次，第二次 3007 走终态）。
  const submit = async (param) => {
    if (submitting || !param || !param.trim()) return;
    submitting = true;
    clearTimeout(tracelessTimer);
    status(t("c.passed"));
    try {
      const r = await invoke("claim_captcha_submit", { param, region });
      if (r && r.retry) {
        submitting = false;
        status(t("c.retrying"));
        await new Promise((res) => setTimeout(res, 600));
        startCaptcha();
        return;
      }
    } catch (e) {
      status(t("c.claimReqFail"), "err");
      detail(stripErr(e));
    }
  };

  const interactive = (why) => {
    notifyStuck();
    clearTimeout(tracelessTimer);
    status(t("c.interactive"));
    $btn.hidden = false;
    $btn.textContent = t("c.retry");
    $btn.focus();
    notify("captcha://setup-failed", { claimId });
  } else if (phase === "interactive") {
    $btn.hidden = false;
    $btn.textContent = t("c.btn");
    $btn.focus();
  } else {
    $btn.hidden = true;
  }
}

  function startCaptcha() {
    try {
      captchaInstance?.destroy?.();
    } catch { }
    captchaInstance = null;
    $btn.hidden = true;
    try {
      window.initAliyunCaptcha({
        SceneId: cfg.scene_id,
        mode: "popup",
        language: lang() === "en" ? "en" : "zh-CN",
        showErrorTip: false,
        element: "#cap-holder",
        button: "#cap-btn",
        getInstance: (instance) => {
          captchaInstance = instance;
          if (typeof instance.startTracelessVerification === "function") {
            instance.startTracelessVerification();
            tracelessTimer = setTimeout(interactive, TRACELESS_TIMEOUT);
          } else {
            interactive();
          }
        },
        success: (param) => submit(typeof param === "string" ? param : param?.captchaVerifyParam),
        fail: (p) => interactive(p),
        onError: (p) => interactive(p),
      });
    } catch (e) {
      notifyStuck();
      status(t("c.initFail"), "err");
      detail(String(e));
    }
  }

  startCaptcha();
}

$btn.addEventListener("click", () => {
  if (!$btn.hidden) status(t("c.inPopup"));
});

$btn.addEventListener("click", () => {
  if (flow.canRetry()) {
    flow.retry();
    return;
  }
  if (!flow.canRetry() && !$btn.hidden) status(t("c.inPopup"));
});

(async () => {
  try {
    const st = await invoke("get_state");
    if (st?.language) init(st.language);
  } catch { }
  document.title = t("c.title");
  $btn.textContent = t("c.retry");
  document.querySelector(".cap-foot").textContent = t("c.foot");
  if (!claimId) {
    status(t("c.cfgUnavailable"), "err");
    detail(t("c.cfgUnavailableDetail"));
    return;
  }
  flow.start();
})();
