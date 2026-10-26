import test from "node:test";
import assert from "node:assert/strict";
import { PERIOD, account, createHarness, tick } from "./ui-harness.mjs";

const ids = (n) => Array.from({ length: n }, (_, i) => account(`a${i}`));

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
