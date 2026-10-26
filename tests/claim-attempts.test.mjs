import test from "node:test";
import assert from "node:assert/strict";
import {
  ATTEMPT_TTL_MS,
  closeWaiter,
  createClaimAttempts,
  finishWaiter,
  pruneAttempts,
  registerAttempt,
  settleAttempt,
  syncClaimAccounts,
  waitForAttempt,
} from "../src/claim-attempts.js";

const accounts = (revision = "r1") => [{ id: "a", quota_revision: revision }, { id: "b", quota_revision: "b1" }];
const payload = (claimId = "c1", extra = {}) => ({ ok: true, claimId, accountId: "a", planName: "Synthetic plan", ...extra });
const base = () => syncClaimAccounts(createClaimAttempts(), accounts());
const register = (model, claimId = "c1", extra = {}) =>
  registerAttempt(model, { claimId, accountId: "a", revision: "r1", planId: "p1", ...extra });

test("a result that arrives before claim_start returns still reaches the caller", async () => {
  const model = base();
  register(model);
  const settled = settleAttempt(model, "c1", payload());
  assert.equal(settled.status, "accepted");
  assert.equal(settled.refresh, true);
  finishWaiter(model, "c1", payload());
  const result = await waitForAttempt(model, "c1");
  assert.equal(result.claimId, "c1");
  assert.equal(model.attempts.get("c1").delivered, true);
});

test("a late success still refreshes the row after the waiter timed out", async () => {
  const model = base();
  register(model);
  const first = await waitForAttempt(model, "c1", { timeoutMs: 5 });
  assert.equal(first, null);
  assert.equal(model.attempts.get("c1").done, true);
  const settled = settleAttempt(model, "c1", payload());
  assert.equal(settled.status, "accepted");
  assert.equal(settled.refresh, true, "the row still matches the attempt generation");
  assert.equal(finishWaiter(model, "c1"), false, "the old waiter is already gone");
});

test("event and status polling deliver one result only", () => {
  const model = base();
  register(model);
  assert.equal(settleAttempt(model, "c1", payload()).status, "accepted");
  assert.equal(settleAttempt(model, "c1", payload()).status, "duplicate");
  assert.equal(model.attempts.get("c1").result.planName, "Synthetic plan");
});

test("an old claim result cannot complete a newer waiter", async () => {
  const model = base();
  register(model, "c1");
  register(model, "c2");
  const waiter = waitForAttempt(model, "c2");
  settleAttempt(model, "c1", payload("c1"));
  finishWaiter(model, "c1", payload("c1"));
  const outcome = await Promise.race([waiter, Promise.resolve("waiting")]);
  assert.equal(outcome, "waiting", "the c2 waiter is still pending");
  settleAttempt(model, "c2", payload("c2"));
  finishWaiter(model, "c2", payload("c2"));
  assert.equal((await waiter).claimId, "c2");
});

test("a deleted account is not refreshed by a late result", () => {
  const model = base();
  register(model);
  const removed = syncClaimAccounts(model, accounts().slice(1));
  const settled = settleAttempt(removed, "c1", payload());
  assert.equal(settled.refresh, false);
  assert.equal(settled.status, "accepted", "the result is still recorded");
});

test("a credential update is not refreshed by a late result", () => {
  const model = base();
  register(model);
  const updated = syncClaimAccounts(model, accounts("r2"));
  assert.equal(settleAttempt(updated, "c1", payload()).refresh, false);
});

test("delete and re-add of the same revision cannot be refreshed", () => {
  const model = base();
  register(model);
  const removed = syncClaimAccounts(model, accounts().slice(1));
  const readded = syncClaimAccounts(removed, accounts());
  assert.equal(settleAttempt(readded, "c1", payload()).refresh, false);
});

test("renaming keeps the account generation", () => {
  const model = base();
  register(model);
  const renamed = syncClaimAccounts(model, accounts().map((a) => ({ ...a, name: "Renamed" })));
  assert.equal(settleAttempt(renamed, "c1", payload()).refresh, true);
});

test("an unknown claim id is ignored", () => {
  const model = base();
  assert.deepEqual(settleAttempt(model, "missing", payload("missing")), {
    status: "unknown_claim",
    refresh: false,
    attempt: null,
  });
});

test("closing a waiter twice is harmless and does not discard the attempt", async () => {
  const model = base();
  register(model);
  const waiter = waitForAttempt(model, "c1", { timeoutMs: 1000 });
  closeWaiter(model, "c1");
  closeWaiter(model, "c1");
  assert.equal(await waiter, null);
  assert.equal(model.attempts.has("c1"), true);
  assert.equal(settleAttempt(model, "c1", payload()).status, "accepted");
});

test("finished attempts are pruned by age and count while pending ones survive", () => {
  const model = base();
  for (let i = 0; i < 120; i++) {
    register(model, `c${i}`);
    settleAttempt(model, `c${i}`, payload(`c${i}`));
  }
  register(model, "live");
  pruneAttempts(model, model.attempts.get("c119").startedAt + ATTEMPT_TTL_MS + 1);
  assert.equal(model.attempts.has("live"), true, "unfinished attempts are never pruned");
  assert.ok(model.attempts.size <= 101);
});
