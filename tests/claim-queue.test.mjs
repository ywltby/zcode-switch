import test from "node:test";
import assert from "node:assert/strict";
import { createClaimQueue } from "../src/claim-queue.js";
import { createClaimFlow } from "../src/claim-flow.js";

const tick = () => new Promise(resolve => setImmediate(resolve));
const account = (id, revision = "r1") => ({ id, quota_revision: revision });
const plan = (id = "p1") => ({ plan_id: id, name: id });
function harness(options = {}) {
  let clock = 1000, blocked = false;
  const started = [], pending = [], outcomes = [];
  const h = { started, pending, outcomes };
  h.queue = createClaimQueue({
    now: () => clock, gapMs: 0, retryDelayMs: 1000,
    canStart: () => !blocked,
    run: job => { started.push(job); return new Promise(resolve => pending.push(resolve)); },
    onSettled: (job, outcome) => outcomes.push({ job, outcome }),
    ...options,
  });
  h.queue.applyAccounts([account("a"), account("b"), account("c")]);
  h.offer = (id, plans = [plan()], revision = "r1") => h.queue.offer({ id, revision, plans });
  h.finish = async (outcome = { status: "ok" }) => { await tick(); assert.ok(pending.length); pending.shift()(outcome); await tick(); };
  h.block = value => { blocked = value; h.queue.kick(); };
  h.advance = ms => { clock += ms; };
  return h;
}

test("a held claim never stalls the three-slot scan and later claims stay serial", async () => {
  const h = harness();
  const replies = new Map();
  const accounts = Array.from({ length: 11 }, (_, i) => account(`a${i}`));
  h.queue.applyAccounts(accounts);
  const flow = createClaimFlow({
    invoke: (_command, { id, revision }) => new Promise(resolve => replies.set(id, () => resolve({ accountId: id, revision, plans: [plan()] }))),
    onResult: result => h.queue.offer(result),
  });
  flow.applyAccounts(accounts);
  let scanDone = false;
  const scan = flow.refreshMany(accounts.map(a => a.id)).then(() => { scanDone = true; });
  assert.equal(replies.size, 3);
  replies.get("a0")(); await tick();
  assert.equal(h.started.length, 1, "the first result starts a claim before the scan finishes");
  assert.equal(scanDone, false);
  for (let i = 1; i < 11; i++) { assert.ok(replies.has(`a${i}`)); replies.get(`a${i}`)(); await tick(); }
  await scan;
  assert.equal(h.started.length, 1, "the held claim has no parallel submission");
  assert.equal(h.queue.pendingCount(), 10);
  await h.finish();
  assert.equal(h.started[1].id, "a1");
});

test("repeated scans and stale post-success eligibility cannot submit a plan twice", async () => {
  const h = harness();
  h.offer("a"); h.offer("a"); h.offer("b"); h.offer("b");
  assert.equal(h.queue.pendingCount(), 1);
  await tick();
  assert.equal(h.started.length, 1);
  await h.finish();
  h.offer("a");
  assert.equal(h.queue.pendingCount(), 0);
  await h.finish();
  assert.equal(h.started.length, 2);
  h.advance(1001); h.offer("a"); await tick();
  assert.equal(h.started.length, 3, "a future fresh scan can try again after the cooldown");
});

test("empty eligibility withdraws queued plans", async () => {
  const h = harness();
  h.offer("a"); h.offer("b"); h.offer("b", []);
  await h.finish();
  assert.equal(h.started.length, 1);
  assert.equal(h.queue.pendingCount(), 0);
});

test("account updates and delete-readd discard queued jobs from the previous generation", async () => {
  const h = harness();
  h.block(true); h.offer("a"); h.offer("b");
  h.queue.applyAccounts([account("a", "r2")]);
  h.queue.applyAccounts([account("a", "r2"), account("b")]);
  h.block(false); await tick();
  assert.equal(h.started.length, 0);
  h.offer("a", [plan()], "r1"); await tick();
  assert.equal(h.started.length, 0, "late results cannot enqueue for the new revision");
  h.offer("a", [plan()], "r2"); await tick();
  assert.equal(h.started[0].revision, "r2");
});

test("renaming preserves queued ownership", async () => {
  const h = harness(); h.block(true); h.offer("a");
  h.queue.applyAccounts([{ ...account("a"), name: "renamed" }]);
  h.block(false); await tick();
  assert.equal(h.started[0].id, "a");
});

test("backend uncertainty holds subsequent claims while new scan results still queue", async () => {
  const h = harness(); h.offer("a"); await tick();
  h.block(true); h.offer("b"); h.offer("c");
  await h.finish({ status: "unknown" });
  assert.equal(h.started.length, 1);
  assert.equal(h.queue.pendingCount(), 2);
  h.block(false); await tick();
  assert.equal(h.started[1].id, "b");
});

test("an unknown account is skipped without blocking other accounts", async () => {
  const h = harness({ allowAccount: id => id !== "a" });
  h.offer("a"); h.offer("b"); await tick();
  assert.deepEqual(h.started.map(x => x.id), ["b"]);
});

test("a failure cools down that account but the next account continues", async () => {
  const h = harness(); h.offer("a", [plan("p1"), plan("p2")]); h.offer("b");
  await h.finish({ status: "failed", cooldownUntil: 5000 });
  assert.equal(h.started[1].id, "b");
  h.offer("a", [plan("p3")]); await h.finish();
  assert.equal(h.started.length, 2);
  h.advance(4001); h.offer("a", [plan("p3")]); await tick();
  assert.equal(h.started[2].id, "a");
});

test("a throwing claim releases its slot and cannot become an unhandled rejection", async () => {
  const started = [];
  const h = harness({ run: async job => { started.push(job.id); if (job.id === "a") throw new Error("offline"); return { status: "ok" }; } });
  h.offer("a"); h.offer("b"); await tick();
  assert.deepEqual(started, ["a", "b"]);
  assert.equal(h.queue.activeId(), null);
  assert.equal(h.outcomes[0].outcome.status, "failed");
});

test("an account cannot consume more than the per-window claim cap", async () => {
  const h = harness({ perAccountLimit: 2 });
  h.offer("a", [plan("p1"), plan("p2"), plan("p3")]);
  await h.finish(); await h.finish();
  h.offer("a", [plan("p3")]); await tick();
  assert.equal(h.started.length, 2);
  h.advance(1001); h.offer("a", [plan("p3")]); await tick();
  assert.equal(h.started.length, 3);
});

test("a new account generation is not cooled down by an old in-flight result", async () => {
  const h = harness(); h.offer("a"); await tick();
  h.queue.applyAccounts([account("a", "r2")]);
  h.offer("a", [plan()], "r2");
  await h.finish({ status: "failed", cooldownUntil: 99999 });
  assert.equal(h.started[1].revision, "r2");
});

test("a manually completed plan is removed from the automatic queue and cannot repeat", async () => {
  const h = harness(); h.block(true); h.offer("a"); h.offer("b");
  h.queue.record({ id: "a", revision: "r1", planId: "p1" }, { status: "ok" });
  h.offer("a"); h.block(false); await tick();
  assert.deepEqual(h.started.map(job => job.id), ["b"]);
});

test("a later cancellation cannot shorten the cooldown for an interactive challenge", async () => {
  const h = harness(); h.offer("a"); await tick();
  h.queue.record({ id: "a", revision: "r1", planId: "p1" }, { status: "failed", cooldownUntil: 10000 });
  await h.finish({ status: "cancelled" });
  h.advance(2000); h.offer("a", [plan("p2")]); await tick();
  assert.equal(h.started.length, 1, "the longer interactive cooldown must survive cancellation");
});

test("deletion before the consumer starts never submits the reserved snapshot", async () => {
  const h = harness(); h.offer("a");
  h.queue.applyAccounts([]);
  await tick();
  assert.equal(h.started.length, 0);
});
