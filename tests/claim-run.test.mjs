import test from "node:test";
import assert from "node:assert/strict";
import * as claimRun from "../src/claim-run.js";
import {
  CHALLENGE_HOLD_MS,
  autoCooldownUntil,
  claimHoldUntil,
  claimOutcomeOf,
  claimSettlement,
  payloadFromStatus,
  resolveClaimTimeout,
  waitForBackendResult,
} from "../src/claim-run.js";

const attempt = { claimId: "c1", accountId: "a" };
const manual = { invoke: null, claimId: "c1", mode: "manual", attempt };

function harness(statuses) {
  const calls = [];
  const handled = [];
  let index = 0;
  const invoke = (command, args) => {
    calls.push({ command, args });
    if (command === "claim_cancel") return Promise.resolve({ phase: "cancelled", cancelled: true });
    if (command === "claim_status") {
      const status = statuses[Math.min(index, statuses.length - 1)];
      index += 1;
      return Promise.resolve(status);
    }
    return Promise.reject(new Error(`unexpected ${command}`));
  };
  return {
    invoke,
    calls,
    handled,
    handleResult: (payload) => handled.push(payload),
    sleep: async () => {},
    now: () => 0,
  };
}

test("a completed session is consumed when the event was lost", async () => {
  const h = harness([{ claimId: "c1", accountId: "a", status: "succeeded", result: { ok: true, planName: "Plan" } }]);
  const outcome = await resolveClaimTimeout({ ...manual, ...h });
  assert.equal(outcome.status, "ok");
  assert.equal(h.handled.length, 1);
  assert.equal(h.handled[0].planName, "Plan");
});

test("a submitting manual claim ends the wait without unlocking the backend", async () => {
  const h = harness([{ claimId: "c1", accountId: "a", status: "submitting" }]);
  let confirming = 0;
  const outcome = await resolveClaimTimeout({ ...manual, ...h, onConfirming: () => { confirming += 1; } });
  assert.equal(outcome.status, "submitting");
  assert.equal(confirming, 1);
  assert.equal(h.handled.length, 0);
  assert.equal(h.calls.filter((c) => c.command === "claim_status").length, 1, "no polling for a manual wait");
});

test("batch waits for an in-flight submission before starting the next account", async () => {
  const h = harness([
    { claimId: "c1", accountId: "a", status: "submitting" },
    { claimId: "c1", accountId: "a", status: "succeeded", result: { ok: true } },
  ]);
  const outcome = await resolveClaimTimeout({ ...manual, ...h, mode: "all" });
  assert.equal(outcome.status, "ok");
  assert.equal(h.handled.length, 1);
  assert.equal(h.calls.filter((c) => c.command === "claim_status").length, 2);
});

test("an unsubmitted session is cancelled at the deadline", async () => {
  const h = harness([{ claimId: "c1", accountId: "a", status: "verifying" }]);
  let timedOut = 0;
  const outcome = await resolveClaimTimeout({ ...manual, ...h, onTimeout: () => { timedOut += 1; } });
  assert.equal(outcome.status, "cancelled");
  assert.equal(timedOut, 1);
  assert.equal(h.calls.filter((c) => c.command === "claim_cancel").length, 1);
});

test("a missing session status becomes unknown instead of a fake failure", async () => {
  const h = harness([null]);
  const outcome = await resolveClaimTimeout({ ...manual, ...h });
  assert.equal(outcome.status, "unknown");
  assert.equal(h.handled.length, 0);
});

test("waiting for the backend gives up as unknown after the deadline", async () => {
  const h = harness([{ claimId: "c1", accountId: "a", status: "submitting" }]);
  let clock = 0;
  const outcome = await waitForBackendResult({
    invoke: h.invoke,
    claimId: "c1",
    mode: "all",
    handleResult: h.handleResult,
    sleep: async () => { clock += 1000; },
    now: () => clock,
  });
  assert.equal(outcome.status, "unknown");
  assert.equal(h.handled.length, 0);
});

test("payload mapping distinguishes success, unconfirmed, and cancelled", () => {
  assert.equal(claimOutcomeOf(payloadFromStatus({ claimId: "c", status: "succeeded" })).status, "ok");
  assert.equal(claimOutcomeOf(payloadFromStatus({ claimId: "c", status: "unknown" })).status, "unknown");
  assert.equal(claimOutcomeOf(payloadFromStatus({ claimId: "c", status: "cancelled" })).status, "cancelled");
  assert.equal(claimOutcomeOf(payloadFromStatus({ claimId: "c", status: "failed", result: { code: 1005 } })).status, "failed");
});

test("reconciliation consumes a lost terminal event after the UI wait ended", async () => {
  const pending = { claimId: "late", done: true, handled: false };
  const active = { claimId: "active", done: false, handled: false };
  const finished = { claimId: "finished", done: true, handled: true };
  const calls = [], handled = [];
  await claimRun.reconcilePendingClaims({
    attempts: [pending, active, finished],
    invoke: async (command, args) => {
      calls.push({ command, ...args });
      return { claimId: args.claimId, accountId: "a", status: "succeeded", result: { ok: true } };
    },
    handleResult: payload => { pending.handled = true; handled.push(payload); },
  });
  assert.deepEqual(calls, [{ command: "claim_status", claimId: "late" }]);
  assert.equal(handled.length, 1);
  assert.equal(handled[0].ok, true);
  assert.equal(pending.handled, true);
});

test("reconciliation keeps missing and still-submitting sessions blocked", async () => {
  const attempts = ["missing", "network", "submitting"].map(claimId => ({ claimId, done: true, handled: false }));
  const handled = [];
  await claimRun.reconcilePendingClaims({
    attempts,
    invoke: async (_, { claimId }) => {
      if (claimId === "network") throw new Error("offline");
      return claimId === "missing" ? null : { claimId, status: "submitting" };
    },
    handleResult: payload => handled.push(payload),
  });
  assert.equal(handled.length, 0);
  assert.ok(attempts.every(a => !a.handled));
  assert.equal(attempts[2].confirming, true);
  assert.equal(attempts[0].confirming, undefined);
});

test("reconciliation does not redeliver a result already received while polling", async () => {
  const pending = { claimId: "late", confirming: true, handled: false };
  const handled = [];
  await claimRun.reconcilePendingClaims({
    attempts: [pending],
    invoke: async () => {
      pending.handled = true;
      return { claimId: "late", status: "cancelled" };
    },
    handleResult: payload => handled.push(payload),
  });
  assert.equal(handled.length, 0);
});

test("terminal results are never retried again", () => {
  const now = 1_700_000_000_000;
  assert.equal(claimHoldUntil({ terminal: true, code: 1001 }, now), Infinity);
  assert.equal(claimHoldUntil({ terminal: true, code: 1002 }, now), Infinity);
  assert.equal(claimHoldUntil({ terminal: true, code: 1003 }, now), Infinity);
  assert.equal(claimHoldUntil({ terminal: true, code: 1005, nextAt: now + 5_000 }, now), now + 5_000);
  assert.equal(
    claimHoldUntil({ terminal: true, code: 1005 }, now),
    now + CHALLENGE_HOLD_MS,
    "没有上游时间时 1005 也用 60 分钟兜底",
  );
});

test("challenge, 3007 and unknown hold for an hour without re-prompting", () => {
  const now = 1_700_000_000_000;
  assert.equal(claimHoldUntil({ challenge: true, code: -2 }, now), now + CHALLENGE_HOLD_MS);
  assert.equal(claimHoldUntil({ code: 3007 }, now), now + CHALLENGE_HOLD_MS);
  assert.equal(claimHoldUntil({ unknown: true, code: -1 }, now), now + CHALLENGE_HOLD_MS);
  assert.equal(claimHoldUntil({}, now), now + CHALLENGE_HOLD_MS);
});

test("account cooldowns follow the upstream time then the scan interval", () => {
  const now = 1_700_000_000_000;
  assert.equal(autoCooldownUntil({ code: 1005, nextAt: now + 9_000 }, now), now + 9_000);
  assert.equal(autoCooldownUntil({ code: 3012 }, now), now + CHALLENGE_HOLD_MS);
  assert.equal(autoCooldownUntil({ code: "interactive" }, now), now + CHALLENGE_HOLD_MS);
  assert.equal(autoCooldownUntil({ code: -1, unknown: true }, now, 600_000), now + 600_000);
});

test("settlement turns a failure into the hold every other path honours", () => {
  const now = 1_700_000_000_000;
  const terminal = claimSettlement({ ok: false, code: 1003, terminal: true }, now, 600_000);
  assert.equal(terminal.status, "failed");
  assert.equal(terminal.holdUntil, Infinity, "1003 等终态必须在结算处写入永久 hold");
  assert.ok(terminal.cooldownUntil > now, "终态也要给账号级冷却");

  const quota = claimSettlement({ ok: false, code: 1005, terminal: true, nextAt: now + 5_000 }, now, 600_000);
  assert.equal(quota.holdUntil, now + 5_000, "1005 用上游时间");

  const challenge = claimSettlement({ ok: false, code: 3007, challenge: true }, now, 600_000);
  assert.equal(challenge.holdUntil, now + CHALLENGE_HOLD_MS);

  const unknown = claimSettlement({ ok: false, unknown: true }, now, 600_000);
  assert.equal(unknown.holdUntil, now + CHALLENGE_HOLD_MS);
  assert.equal(unknown.cooldownUntil, now + CHALLENGE_HOLD_MS, "unknown 的账号冷却是一小时");
});

test("a success clears the hold instead of extending it", () => {
  const now = 1_700_000_000_000;
  const ok = claimSettlement({ ok: true, planId: "p1" }, now, 600_000);
  assert.equal(ok.status, "ok");
  assert.equal(ok.holdUntil, 0);
  assert.equal(ok.cooldownUntil, 0);
});

test("a result without a payload is never success", () => {
  for (const empty of [undefined, null, {}]) {
    assert.equal(claimOutcomeOf(empty).status, "failed", `空载荷 ${JSON.stringify(empty)} 不能当成功`);
  }
  assert.equal(claimOutcomeOf({ ok: true }).status, "ok");
  assert.equal(claimOutcomeOf({ ok: false, code: 1003, terminal: true }).status, "failed");
});

test("a preparation failure without payload still gets a cooldown", () => {
  const now = 1_700_000_000_000;
  const settled = claimSettlement(undefined, now, 600_000);
  assert.equal(settled.status, "failed");
  assert.ok(settled.holdUntil > now, "无载荷失败也必须写入 hold，避免立即重复启动");
  assert.ok(settled.cooldownUntil >= now, "无载荷失败也要有账号冷却");
});
