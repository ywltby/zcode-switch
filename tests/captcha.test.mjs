import test from "node:test";
import assert from "node:assert/strict";
import { createCaptchaFlow, INIT_TIMEOUT_MS, SDK_TIMEOUT_MS } from "../src/captcha-flow.js";

const tick = () => new Promise(setImmediate);

function fakeTimers() {
  const timers = new Map();
  let id = 0;
  return {
    setTimer(fn, ms) { timers.set(++id, { fn, ms }); return id; },
    clearTimer(timer) { timers.delete(timer); },
    fireAll() {
      const pending = [...timers.entries()];
      timers.clear();
      for (const [, timer] of pending) timer.fn();
    },
    pending: () => timers.size,
  };
}

function config(overrides = {}) {
  return { enabled: true, region: "cn", prefix: "synthetic", scene_id: "scene-1", ...overrides };
}

function harness({ status = { status: "verifying" }, configResult = config(), sdk, init, auto = false, timers = fakeTimers() } = {}) {
  const calls = [];
  const order = [];
  const phases = [];
  const submits = [];
  const interactive = [];
  const outcomes = [];
  let sdkGate = sdk;
  let initGate = init;
  const invoke = (command, args) => {
    calls.push({ command, args });
    if (command === "claim_status") return Promise.resolve(status);
    if (command === "claim_captcha_config") {
      return configResult instanceof Error ? Promise.reject(configResult) : Promise.resolve(configResult);
    }
    if (command === "claim_captcha_submit") {
      submits.push(args);
      return Promise.resolve({ ok: true, claimId: args.claimId });
    }
    return Promise.reject(new Error(`unexpected ${command}`));
  };
  const flow = createCaptchaFlow({
    invoke,
    claimId: "claim-1",
    auto,
    loadSdk: () => {
      order.push("sdk");
      return sdkGate ? sdkGate() : Promise.resolve();
    },
    initCaptcha: (options) => {
      order.push("init");
      return initGate ? initGate(options) : undefined;
    },
    applyConfig: () => order.push("config"),
    onStatus: (phase) => phases.push(phase),
    onInteractive: (why) => interactive.push(why),
    onOutcome: (outcome) => outcomes.push(outcome),
    setTimer: timers.setTimer,
    clearTimer: timers.clearTimer,
  });
  void calls;
  return { flow, calls, order, phases, submits, interactive, outcomes, timers, invoke };
}

test("the captcha config is applied before the SDK script loads", async () => {
  const h = harness({ init: (options) => options.onInstance({ startTracelessVerification() {} }) });
  await h.flow.start();
  assert.deepEqual(h.order, ["config", "sdk", "init"]);
});

test("a stuck SDK becomes a setup failure and can be retried", async () => {
  let resolveSdk;
  const h = harness({ sdk: () => new Promise((resolve) => { resolveSdk = resolve; }) });
  const started = h.flow.start();
  await tick();
  assert.equal(h.flow.phase(), "preparing");
  h.timers.fireAll();
  await started;
  assert.equal(h.flow.phase(), "setup_failed");
  assert.equal(h.flow.canRetry(), true);
  resolveSdk();
  await tick();
  assert.equal(h.flow.phase(), "setup_failed");
});

test("a stuck captcha initialization is bounded", async () => {
  const h = harness({ init: () => {} });
  await h.flow.start();
  assert.equal(h.flow.phase(), "verifying");
  h.timers.fireAll();
  assert.equal(h.flow.phase(), "setup_failed");
});

test("a late success after the setup timeout cannot submit", async () => {
  let lateSuccess;
  const h = harness({
    init: (options) => { lateSuccess = options.success; },
  });
  await h.flow.start();
  h.timers.fireAll();
  assert.equal(h.flow.phase(), "setup_failed");
  await lateSuccess("late-param");
  assert.equal(h.submits.length, 0);
});

test("a duplicate success callback submits exactly once", async () => {
  let success;
  const h = harness({ init: (options) => { success = options.success; options.onInstance({}); } });
  await h.flow.start();
  await success("param-1");
  await success("param-1");
  assert.equal(h.submits.length, 1);
  assert.equal(h.submits[0].param, "param-1");
  assert.equal(h.submits[0].claimId, "claim-1");
  assert.equal(h.flow.phase(), "succeeded");
});

test("a reload that finds a submitting session never initializes the captcha", async () => {
  const h = harness({ status: { status: "submitting" }, init: (options) => options.onInstance({}) });
  await h.flow.start();
  assert.equal(h.flow.phase(), "already");
  assert.equal(h.calls.filter((c) => c.command === "claim_captcha_config").length, 0);
  assert.equal(h.submits.length, 0);
});

test("a retry after a setup failure uses a fresh generation", async () => {
  let attempts = 0;
  let firstSuccess;
  const h = harness({
    init: (options) => {
      attempts += 1;
      if (attempts === 1) firstSuccess = options.success;
      else options.onInstance({});
    },
  });
  await h.flow.start();
  h.timers.fireAll();
  assert.equal(h.flow.phase(), "setup_failed");
  await firstSuccess("stale-param");
  assert.equal(h.submits.length, 0);
  h.flow.retry();
  await tick();
  await tick();
  assert.equal(attempts, 2);
  assert.equal(h.flow.generation(), 3, "the failed attempt and the retry each bumped the generation");
});

test("an interactive challenge is separated from a setup failure", async () => {
  const h = harness({ init: (options) => options.onInstance({ startTracelessVerification() {} }) });
  await h.flow.start();
  h.timers.fireAll();
  assert.equal(h.flow.phase(), "interactive");
  assert.deepEqual(h.interactive, ["traceless"]);
  assert.equal(h.flow.canRetry(), false, "interactive must not offer a setup retry");
});

test("automatic verification blocks success callbacks as soon as interaction is required", async () => {
  let callbacks;
  const h = harness({ auto: true, init: (options) => { callbacks = options; } });
  await h.flow.start();
  callbacks.fail("interactive");
  await callbacks.success("late-param");
  assert.deepEqual(h.interactive, ["interactive"]);
  assert.equal(h.submits.length, 0);
  assert.equal(h.flow.phase(), "interactive");
});

test("manual interactive verification can still submit", async () => {
  let callbacks;
  const h = harness({ init: (options) => { callbacks = options; } });
  await h.flow.start();
  callbacks.fail("interactive");
  await callbacks.success("manual-param");
  assert.equal(h.submits.length, 1);
  assert.equal(h.flow.phase(), "succeeded");
});

test("a synchronous verification success cannot later become interactive", async () => {
  const h = harness({
    auto: true,
    init: (options) => options.onInstance({
      startTracelessVerification() { void options.success("immediate-param"); },
    }),
  });
  await h.flow.start();
  await tick();
  h.timers.fireAll();
  assert.equal(h.submits.length, 1);
  assert.deepEqual(h.interactive, []);
  assert.equal(h.flow.phase(), "succeeded");
});

test("a config failure is reported as a setup failure", async () => {
  const h = harness({ configResult: new Error("config_unavailable") });
  await h.flow.start();
  assert.equal(h.flow.phase(), "setup_failed");
  assert.equal(h.order.includes("sdk"), false);
});

test("bounded setup waits keep their configured durations", () => {
  assert.equal(SDK_TIMEOUT_MS, 12_000);
  assert.equal(INIT_TIMEOUT_MS, 12_000);
});
