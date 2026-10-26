import test from "node:test";
import assert from "node:assert/strict";
import {
  claimOutcomeOf,
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
