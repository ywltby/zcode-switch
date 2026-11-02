// Captcha preparation flow: config before SDK, bounded SDK/init waits, and
// generation-scoped callbacks so a late success can never submit.
// DOM and Tauri details stay in captcha.js.

export const SDK_TIMEOUT_MS = 12_000;
export const INIT_TIMEOUT_MS = 12_000;
export const TRACELESS_TIMEOUT_MS = 8_000;

export function createCaptchaFlow({
  invoke,
  claimId,
  auto = false,
  loadSdk,
  initCaptcha,
  applyConfig = () => {},
  onStatus = () => {},
  onDetail = () => {},
  onInteractive = () => {},
  onOutcome = () => {},
  setTimer = setTimeout,
  clearTimer = clearTimeout,
}) {
  let generation = 0;
  let phase = "idle";
  let submitted = false;
  let region = null;
  let tracelessTimer = 0;

  function setPhase(next) {
    phase = next;
    onStatus(next);
  }

  function failedSetup(gen, detail) {
    if (gen !== generation) return;
    // Invalidate callbacks from this attempt: a late success must not submit.
    generation += 1;
    setPhase("setup_failed");
    if (detail) onDetail(detail);
  }

  function withTimeout(promise, ms) {
    return new Promise((resolve, reject) => {
      const timer = setTimer(() => reject(new Error("timeout")), ms);
      Promise.resolve(promise).then(
        (value) => { clearTimer(timer); resolve(value); },
        (error) => { clearTimer(timer); reject(error); },
      );
    });
  }

  async function start() {
    const gen = ++generation;
    submitted = false;
    region = null;
    setPhase("preparing");
    onDetail("");
    let status = null;
    try {
      status = await invoke("claim_status", { claimId });
    } catch {
      status = null;
    }
    if (gen !== generation) return;
    if (status && !["preparing", "verifying"].includes(status.status)) {
      // Reloading the window must not re-initialize a session that already submitted.
      setPhase("already");
      return;
    }
    let config = null;
    try {
      config = await invoke("claim_captcha_config");
    } catch (error) {
      failedSetup(gen, String(error));
      return;
    }
    if (gen !== generation) return;
    if (!config?.enabled || !config?.scene_id) {
      failedSetup(gen, "disabled");
      return;
    }
    region = config.region || null;
    // The SDK reads this global during script evaluation.
    applyConfig(config);
    try {
      await withTimeout(loadSdk(), SDK_TIMEOUT_MS);
    } catch {
      failedSetup(gen, "sdk");
      return;
    }
    if (gen !== generation) return;
    setPhase("verifying");
    let initTimer = setTimer(() => failedSetup(gen, "init"), INIT_TIMEOUT_MS);
    const clearInit = () => {
      if (initTimer) {
        clearTimer(initTimer);
        initTimer = 0;
      }
    };

    const interactive = (why) => {
      if (gen !== generation || submitted) return;
      clearInit();
      clearTimer(tracelessTimer);
      // Stop auto callbacks before the asynchronous parent-window cancellation.
      // Manual challenges must remain able to submit after user interaction.
      if (auto) generation += 1;
      setPhase("interactive");
      onInteractive(why);
    };

    const handleSuccess = async (param) => {
      if (gen !== generation || submitted) return;
      const value = typeof param === "string" ? param : param?.captchaVerifyParam;
      if (!value || !value.trim()) return;
      submitted = true;
      clearInit();
      clearTimer(tracelessTimer);
      setPhase("submitted");
      let outcome = null;
      try {
        outcome = await invoke("claim_captcha_submit", { claimId, param: value, region });
      } catch (error) {
        if (gen === generation) {
          setPhase("submit_failed");
          onDetail(String(error));
        }
        return;
      }
      if (gen !== generation) return;
      onOutcome(outcome);
      setPhase(outcome?.unknown ? "unknown" : outcome?.ok === false ? "failed" : "succeeded");
    };

    try {
      initCaptcha({
        sceneId: config.scene_id,
        region: config.region,
        onInstance: (instance) => {
          if (gen !== generation) return;
          clearInit();
          if (instance && typeof instance.startTracelessVerification === "function") {
            tracelessTimer = setTimer(() => interactive("traceless"), TRACELESS_TIMEOUT_MS);
            instance.startTracelessVerification();
          } else {
            interactive();
          }
        },
        success: handleSuccess,
        fail: (why) => interactive(why),
        onError: (why) => interactive(why),
      });
    } catch (error) {
      clearInit();
      failedSetup(gen, String(error));
    }
  }

  function retry() {
    if (phase !== "setup_failed") return false;
    void start();
    return true;
  }

  return {
    start,
    retry,
    canRetry: () => phase === "setup_failed",
    phase: () => phase,
    generation: () => generation,
  };
}
