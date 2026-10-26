
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { init, t, lang, stripErr } from "./i18n.js";
import { createCaptchaFlow } from "./captcha-flow.js";

const SDK_URL = "https://o.alicdn.com/captcha-frontend/aliyunCaptcha/AliyunCaptcha.js";
const claimId = new URLSearchParams(location.search).get("claimId") || "";

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

let flow = null;

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

function onStatus(phase) {
  const label = labels[phase];
  const tone = phase === "succeeded" ? "ok" : ["failed", "setup_failed", "submit_failed", "unknown"].includes(phase) ? "err" : "run";
  if (label) status(label(), tone);
  if (phase === "setup_failed") {
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

flow = createCaptchaFlow({
  invoke,
  claimId,
  loadSdk,
  applyConfig: (config) => {
    // Must be set before the SDK evaluates its configuration.
    window.AliyunCaptchaConfig = { region: config.region, prefix: config.prefix };
  },
  initCaptcha: ({ sceneId, region, onInstance, success, fail, onError }) => {
    window.initAliyunCaptcha({
      SceneId: sceneId,
      mode: "popup",
      language: lang() === "en" ? "en" : "zh-CN",
      showErrorTip: false,
      element: "#cap-holder",
      button: "#cap-btn",
      getInstance: (instance) => onInstance(instance),
      success: (param) => success(typeof param === "string" ? param : param?.captchaVerifyParam),
      fail: (payload) => fail(payload),
      onError: (payload) => onError(payload),
    });
  },
  onStatus,
  onDetail: (text) => detail(text),
  onInteractive: (why) => {
    notify("captcha://interactive", { claimId });
    if (why) detail(typeof why === "string" ? why.slice(0, 120) : JSON.stringify(why).slice(0, 120));
  },
  onOutcome: (outcome) => {
    if (outcome?.unknown) detail(t("c.unknownDetail"));
    else if (outcome?.ok === false) detail(stripErr(outcome?.message || ""));
  },
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
