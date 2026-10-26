import test from "node:test";
import assert from "node:assert/strict";
import {
  beginClaimRequest,
  createClaimState,
  invalidateClaimEntry,
  releaseClaimRequest,
  settleClaimRequest,
  syncClaimAccounts,
} from "../src/claim-state.js";

const accounts = (revision = "r1") => [{ id: "a", quota_revision: revision }, { id: "b", quota_revision: "b1" }];
const plans = (name = "Synthetic plan") => [{ plan_id: "p1", name, grants: [] }];
const initial = () => syncClaimAccounts(createClaimState(), accounts());

test("a preview result lands only for the current ticket", () => {
  const first = beginClaimRequest(initial(), "a", "preview");
  assert.equal(first.ticket.kind, "preview");
  const settled = settleClaimRequest(first.model, first.ticket, { plans: plans() });
  assert.deepEqual(settled.entries.a.plans, plans());
  assert.equal(settled.entries.a.busy, false);
  assert.strictEqual(settleClaimRequest(settled, first.ticket, { plans: plans("late") }), settled);
});

test("credential update rejects an older preview and keeps the row empty", () => {
  const first = beginClaimRequest(initial(), "a", "refresh");
  const updated = syncClaimAccounts(first.model, accounts("r2"));
  assert.equal(updated.entries.a.plans.length, 0);
  assert.strictEqual(settleClaimRequest(updated, first.ticket, { plans: plans() }), updated);
});

test("delete and re-add of the same revision still rejects the earlier result", () => {
  const first = beginClaimRequest(initial(), "a", "preview");
  const removed = syncClaimAccounts(first.model, accounts().slice(1));
  const readded = syncClaimAccounts(removed, accounts());
  assert.equal(readded.entries.a.plans.length, 0);
  assert.strictEqual(settleClaimRequest(readded, first.ticket, { plans: plans() }), readded);
  const next = beginClaimRequest(readded, "a", "preview");
  assert.notEqual(next.ticket.sequence, first.ticket.sequence);
});

test("a failed refresh keeps the old plans and records the error", () => {
  const first = beginClaimRequest(initial(), "a", "refresh");
  const loaded = settleClaimRequest(first.model, first.ticket, { plans: plans() });
  const second = beginClaimRequest(loaded, "a", "refresh");
  const failed = settleClaimRequest(second.model, second.ticket, { error: "network_error" });
  assert.deepEqual(failed.entries.a.plans, plans());
  assert.equal(failed.entries.a.err, "network_error");
  assert.equal(failed.entries.a.busy, false);
});

test("activation failure with a successful preview keeps the plans and reports activation", () => {
  const first = beginClaimRequest(initial(), "a", "refresh");
  const settled = settleClaimRequest(first.model, first.ticket, { plans: plans(), activationError: "denied" });
  assert.deepEqual(settled.entries.a.plans, plans());
  assert.equal(settled.entries.a.activationError, "denied");
  assert.equal(settled.entries.a.err, null);
});

test("a busy row refuses a second request until released", () => {
  const first = beginClaimRequest(initial(), "a", "preview");
  assert.equal(beginClaimRequest(first.model, "a", "refresh").ticket, null);
  const released = releaseClaimRequest(first.model, first.ticket);
  assert.equal(released.entries.a.busy, false);
  assert.ok(beginClaimRequest(released, "a", "refresh").ticket);
});

test("cancelling a queued preview frees the row without touching its plans", () => {
  const first = beginClaimRequest(initial(), "a", "preview");
  const cancelled = releaseClaimRequest(first.model, first.ticket);
  const stale = settleClaimRequest(cancelled, first.ticket, { plans: plans("stale") });
  assert.deepEqual(stale.entries.a.plans, []);
});

test("explicit refresh invalidates plans that were started before a claim", () => {
  const first = beginClaimRequest(initial(), "a", "preview");
  const loaded = settleClaimRequest(first.model, first.ticket, { plans: plans() });
  const cleared = invalidateClaimEntry(loaded, "a");
  assert.deepEqual(cleared.entries.a.plans, []);
  assert.equal(cleared.entries.a.busy, false);
  assert.deepEqual(cleared.entries.b, loaded.entries.b);
});
