import test from "node:test";
import assert from "node:assert/strict";
import { PERIOD, account, createClaimHarness, createHarness, tick } from "./ui-harness.mjs";
import { createRequestQueue } from "../src/request-queue.js";
import { createClaimFlow } from "../src/claim-flow.js";

const ids = (n) => Array.from({ length: n }, (_, i) => account(`a${i}`));

test("accepted eligibility is published while the rest of the scan is still pending", async () => {
  const replies = new Map();
  const published = [];
  const flow = createClaimFlow({
    invoke: (_command, { id, revision }) => new Promise((resolve) => replies.set(id, (plans) => resolve({ accountId: id, revision, plans }))),
    onResult: (result) => { published.push(result); },
  });
  flow.applyAccounts(ids(4));
  const scan = flow.refreshMany(ids(4).map(a => a.id));
  replies.get("a0")([{ plan_id: "p" }]);
  await tick();
  assert.deepEqual(published.map(r => [r.id, r.revision, r.plans[0]?.plan_id]), [["a0", "r1", "p"]]);
  assert.ok(replies.has("a3"), "a completed query immediately releases its scan slot");
  for (const id of ["a1", "a2", "a3"]) replies.get(id)([]);
  await scan;
  assert.equal(published.length, 4, "empty successful results also withdraw queued eligibility");
});

test("eligibility notifications exclude failures and old account generations", async () => {
  let resolve;
  const published = [];
  const flow = createClaimFlow({
    invoke: (_command, args) => new Promise(r => { resolve = plans => r({ accountId: args.id, revision: args.revision, plans }); }),
    onResult: result => published.push(result),
  });
  flow.applyAccounts([account("a")]);
  const stale = flow.refresh("a");
  flow.applyAccounts([]);
  flow.applyAccounts([account("a")]);
  resolve([{ plan_id: "old" }]);
  await stale;
  assert.equal(published.length, 0);
  const fresh = flow.refresh("a");
  const reused = flow.refresh("a");
  resolve([{ plan_id: "new" }]);
  await Promise.all([fresh, reused]);
  assert.equal(published.length, 1, "reused requests publish once");
  const failed = createClaimFlow({invoke: async () => { throw new Error("offline"); }, onResult: result => published.push(result)});
  failed.applyAccounts([account("a")]);
  await failed.refresh("a");
  assert.equal(published.length, 1, "failures cannot enqueue cached plans");
});

test("twenty due accounts drain through at most three slots", async () => {
  const h = createHarness({ accounts: ids(20) });
  h.holdAll();
  h.scheduler.sweep();
  assert.equal(h.active(), 3, "only three IPC calls may be in flight");
  await h.drain();
  assert.equal(h.peak(), 3);
  const calls = h.control.calls.filter((c) => c.command === "get_account_quota");
  assert.equal(calls.length, 20, "each account refreshes exactly once");
  for (const a of h.accounts()) assert.ok(h.state(a.id)?.data, `${a.id} received its quota`);
});

test("manual duplicate promotes the queued request without a second IPC call", async () => {
  const h = createHarness({ accounts: ids(5), limit: 1 });
  h.holdAll();
  h.scheduler.sweep();
  const first = h.scheduler.request("a4", { priority: "manual" });
  const second = h.scheduler.request("a4", { priority: "manual" });
  assert.equal(first, second);
  h.release("a0");
  await tick();
  const order = h.control.calls.filter((c) => c.command === "get_account_quota").map((c) => c.args.id);
  assert.deepEqual(order.slice(0, 2), ["a0", "a4"]);
  assert.equal(h.callsTo("a4").length, 1);
  await h.drain();
});

test("a late success from an old revision is discarded and the new revision refreshes", async () => {
  const h = createHarness({ accounts: [account("a")] });
  h.control.hold.add("a");
  const stale = h.scheduler.request("a");
  await tick();
  h.bumpRevision("a", "r2");
  assert.equal(h.state("a").busy, false, "revision change releases the pending ticket");
  h.release("a");
  assert.equal(await stale, undefined);
  assert.equal(h.state("a").data, undefined, "old success must not land in the new row");
  h.control.hold.delete("a");
  await h.scheduler.request("a");
  assert.equal(h.state("a").revision, "r2");
  assert.ok(h.state("a").data);
});

test("a late failure from an old revision is discarded", async () => {
  const h = createHarness({ accounts: [account("a")] });
  h.control.hold.add("a");
  const stale = h.scheduler.request("a");
  await tick();
  h.bumpRevision("a", "r2");
  h.fail("a", new Error("network_error"));
  await stale;
  assert.equal(h.state("a").err, undefined);
  assert.equal(h.state("a").data, undefined);
});

test("deleting a queued account cancels it before it reaches IPC", async () => {
  const h = createHarness({ accounts: ids(3), limit: 1 });
  h.holdAll();
  h.scheduler.sweep();
  h.remove("a1");
  assert.equal(h.callsTo("a1").length, 0);
  h.release("a0");
  await tick();
  assert.equal(h.callsTo("a1").length, 0);
  assert.equal(h.callsTo("a2").length, 1, "the freed slot goes to the surviving account");
  await h.drain();
});

test("delete and re-add with the same revision cannot accept the earlier response", async () => {
  const h = createHarness({ accounts: [account("a")] });
  h.control.hold.add("a");
  const stale = h.scheduler.request("a");
  await tick();
  h.remove("a");
  h.readd("a", "r1");
  h.release("a");
  await stale;
  assert.equal(h.state("a").data, undefined);
  h.control.hold.delete("a");
  await h.scheduler.request("a");
  assert.ok(h.state("a").data, "the fresh request for the re-added account succeeds");
  assert.equal(h.callsTo("a").length, 2);
});

test("renaming keeps the cached quota and the pending request", async () => {
  const h = createHarness({ accounts: [account("a")] });
  await h.scheduler.request("a");
  const cached = h.state("a").data;
  const due = h.scheduler.dueAt("a");
  h.rename("a", "Renamed");
  assert.deepEqual(h.state("a").data, cached);
  assert.equal(h.scheduler.dueAt("a"), due, "renaming does not reschedule the sweep");
  await h.scheduler.request("a");
  assert.equal(h.callsTo("a").length, 2);
});

test("a failure keeps the previous value, releases the slot, and schedules the next sweep", async () => {
  const h = createHarness({ accounts: [account("a"), account("b")], limit: 1 });
  await h.scheduler.request("a");
  const before = h.state("a").data;
  h.control.failures.set("a", true);
  const failing = h.scheduler.request("a");
  await tick();
  h.fail("a", "network_error");
  await failing;
  assert.deepEqual(h.state("a").data, before);
  assert.equal(h.state("a").err, "network_error");
  assert.equal(h.state("a").busy, false);
  await h.scheduler.request("b");
  assert.ok(h.state("b").data, "the released slot serves the next account");
  assert.equal(h.scheduler.dueAt("a"), PERIOD, "completed refreshes schedule the next sweep");
});

test("an old task cannot postpone the sweep of a changed revision", async () => {
  const h = createHarness({ accounts: [account("a")] });
  h.control.hold.add("a");
  const stale = h.scheduler.request("a");
  await tick();
  h.bumpRevision("a", "r2");
  const rescheduled = h.scheduler.dueAt("a");
  h.release("a");
  await stale;
  assert.equal(h.scheduler.dueAt("a"), rescheduled, "the stale finally must not overwrite the new due time");
});

test("an idle credential change refreshes on the next sweep without waiting for the old cadence", async () => {
  const h = createHarness({ accounts: [account("a"), account("b")] });
  await h.scheduler.request("a");
  await h.scheduler.request("b");
  h.advance(1000);
  h.bumpRevision("a", "r2");
  assert.equal(h.state("a").data, undefined, "old identity quota is cleared");
  assert.equal(h.scheduler.dueAt("a"), 1000, "the new identity is immediately due");
  assert.equal(h.scheduler.dueAt("b"), PERIOD, "other accounts keep their cadence");
  assert.equal(h.scheduler.sweep(), 1);
  await tick();
  assert.equal(h.state("a").data.revision, "r2");
  assert.equal(h.callsTo("a").length, 2);
});

test("sweep does not duplicate an in-flight manual refresh", async () => {
  const h = createHarness({ accounts: ids(3), limit: 3 });
  h.holdAll();
  const manual = h.scheduler.request("a0", { priority: "manual" });
  h.scheduler.sweep();
  assert.equal(h.callsTo("a0").length, 1);
  assert.equal(h.callsTo("a1").length, 1);
  assert.equal(h.callsTo("a2").length, 1);
  await h.drain();
  await manual;
});

test("sweep skips blocked accounts without losing their due time", async () => {
  const h = createHarness({ accounts: ids(2) });
  h.holdAll();
  const blocked = new Set(["a1"]);
  h.scheduler.sweep({ isBlocked: (id) => blocked.has(id) });
  assert.equal(h.callsTo("a1").length, 0);
  assert.equal(h.callsTo("a0").length, 1);
  await h.drain();
  assert.equal(h.scheduler.dueAt("a1"), 0, "a blocked account stays due for the next tick");
});

// ---- Claim eligibility refresh (Task 3) ----

const claimIds = (n) => Array.from({ length: n }, (_, i) => account(`c${i}`));

test("a preview in flight makes an explicit refresh wait its turn", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.hold.add("c0");
  const preview = h.flow.preview("c0");
  const refresh = h.flow.refresh("c0");
  assert.deepEqual(h.callsTo("c0").map((c) => c.command), ["claim_preview"]);
  h.release("c0");
  await tick();
  assert.deepEqual(h.callsTo("c0").map((c) => c.command), ["claim_preview", "claim_refresh"]);
  h.release("c0");
  const result = await refresh;
  assert.equal(result.status, "ok");
  assert.equal((await preview).status, "ok");
});

test("a refresh in flight lets a manual preview reuse its result", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.hold.add("c0");
  const refresh = h.flow.refresh("c0");
  const preview = h.flow.preview("c0");
  assert.equal(preview, refresh, "the preview must share the in-flight refresh promise");
  h.release("c0");
  assert.equal((await preview).status, "ok");
  assert.equal(h.callsTo("c0").length, 1);
});

test("activation failure does not discard a successful preview", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.responses.c0 = { plans: [{ plan_id: "p1", name: "Grant" }], activationError: "denied" };
  const result = await h.flow.refresh("c0");
  assert.equal(result.status, "ok");
  assert.equal(result.activationError, "denied");
  assert.deepEqual(h.state("c0").plans, [{ plan_id: "p1", name: "Grant" }]);
  assert.equal(h.state("c0").activationError, "denied");
});

test("a failing account does not stop the batch and progress counts each item once", async () => {
  const h = createClaimHarness({ accounts: claimIds(3) });
  h.control.failures.set("c1", [new Error("network_error")]);
  const progress = [];
  const done = await h.flow.refreshMany(["c0", "c1", "c2"], {
    onItem: ({ id, done, total, outcome }) => progress.push({ id, done, total, status: outcome.status }),
  });
  assert.equal(done.length, 3);
  assert.deepEqual(progress.map((p) => p.done), [1, 2, 3]);
  assert.deepEqual(progress.map((p) => p.status), ["ok", "failed", "ok"]);
  assert.equal(h.state("c1").err, "Error: network_error");
  assert.ok(h.state("c2").plans.length, "the batch continues after a failure");
});

test("a late refresh result for a changed revision is discarded", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.hold.add("c0");
  const pending = h.flow.refresh("c0");
  h.bumpRevision("c0", "r2");
  h.release("c0");
  const outcome = await pending;
  assert.equal(outcome.status, "skipped");
  assert.deepEqual(h.state("c0").plans, []);
});

test("scope_changed before any network work retries once after refreshing state", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.failures.set("c0", [h.scopeError()]);
  const outcome = await h.flow.refresh("c0");
  assert.equal(outcome.status, "ok");
  assert.equal(h.control.refreshStateCalls, 1);
  assert.equal(h.callsTo("c0").length, 2);
});

test("a second scope_changed is reported instead of retrying forever", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.failures.set("c0", [h.scopeError(), h.scopeError()]);
  const outcome = await h.flow.refresh("c0");
  assert.equal(outcome.status, "scope_changed");
  assert.equal(h.control.refreshStateCalls, 1);
  assert.equal(h.callsTo("c0").length, 2);
});

test("quota and claim eligibility share the same three slots", async () => {
  const queue = createRequestQueue(3);
  const h = createHarness({ accounts: ids(6), queue });
  const c = createClaimHarness({ accounts: [account("c0"), account("c1")], queue });
  h.holdAll();
  for (const id of ["c0", "c1"]) c.control.hold.add(id);
  h.scheduler.sweep();
  assert.equal(queue.activeCount(), 3, "quota fills the shared slots");
  const claim = c.flow.refresh("c0");
  assert.equal(queue.activeCount(), 3);
  assert.equal(c.callsTo("c0").length, 0, "the claim waits behind quota instead of opening a fourth slot");
  assert.ok(queue.queuedCount() >= 4, "three quota tasks and the claim are queued");
  h.release("a0");
  await tick();
  assert.equal(queue.activeCount(), 3, "the freed slot starts the claim refresh");
  assert.equal(c.callsTo("c0").length, 1);
  assert.equal(h.callsTo("a3").length, 0, "the manual claim took the freed slot");
  c.release("c0");
  await claim;
  await h.drain();
  assert.ok(queue.activeCount() <= 3);
  assert.equal(queue.activeCount(), 0);
});

test("batch eligibility refresh runs three accounts at a time", async () => {
  const h = createClaimHarness({ accounts: claimIds(6) });
  for (const id of ["c0", "c1", "c2", "c3", "c4", "c5"]) h.control.hold.add(id);
  const progress = [];
  const batch = h.flow.refreshMany(["c0", "c1", "c2", "c3", "c4", "c5"], {
    onItem: ({ done, total, outcome }) => progress.push({ done, total, status: outcome.status }),
  });
  assert.equal(h.control.pending.length, 3, "three refreshes start together");
  while (h.control.pending.length) {
    h.release(h.control.pending[0].id);
    await tick();
  }
  const outcomes = await batch;
  assert.equal(outcomes.length, 6);
  assert.deepEqual(progress.map((p) => p.done), [1, 2, 3, 4, 5, 6]);
  assert.deepEqual(progress.map((p) => p.total), [6, 6, 6, 6, 6, 6]);
  assert.equal(progress.filter((p) => p.status === "ok").length, 6);
});

test("two consecutive scope_changed results never leave the row busy", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.failures.set("c0", [h.scopeError(), h.scopeError()]);
  const outcome = await h.flow.refresh("c0");
  assert.equal(outcome.status, "scope_changed");
  assert.equal(h.state("c0").busy, false, "the failed attempt releases its ownership");
  const next = await h.flow.refresh("c0");
  assert.equal(next.status, "ok", "a later refresh still works");
  assert.equal(h.callsTo("c0").length, 3);
  assert.equal(h.control.refreshStateCalls, 1);
});

test("a claim completion invalidates a preview started before submission", async () => {
  const h = createClaimHarness({ accounts: [account("c0")] });
  h.control.hold.add("c0");
  const beforeSubmit = h.flow.preview("c0");
  const after = h.flow.afterClaim("c0");
  assert.notEqual(after, beforeSubmit, "the post-claim preview must be a new request");
  assert.equal(h.callsTo("c0").length, 2);
  h.release("c0");
  assert.equal((await beforeSubmit).status, "skipped");
  h.release("c0");
  const outcome = await after;
  assert.equal(outcome.status, "ok");
  assert.ok(h.state("c0").plans.length);
});
