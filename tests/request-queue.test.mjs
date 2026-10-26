import test from "node:test";
import assert from "node:assert/strict";
import { CANCELLED, createRequestQueue } from "../src/request-queue.js";

// A controlled task: the test decides when it settles, so no real timers.
function controlled() {
  let release, fail;
  const promise = new Promise((resolve, reject) => { release = resolve; fail = reject; });
  return { task: () => promise, release, fail };
}

const tick = () => new Promise(setImmediate);

test("peak concurrency never exceeds the limit and freed slots are reused", async () => {
  const queue = createRequestQueue(3);
  const runs = [];
  let active = 0;
  let peak = 0;
  const make = (label) => async () => {
    active++;
    peak = Math.max(peak, active);
    runs.push(label);
    await new Promise((resolve) => setTimeout(resolve, 0));
    active--;
    return label;
  };
  const promises = ["a", "b", "c", "d", "e", "f", "g"].map((label) => queue.enqueue(label, make(label)));
  assert.deepEqual(await Promise.all(promises), ["a", "b", "c", "d", "e", "f", "g"]);
  assert.equal(peak, 3);
  assert.deepEqual(runs, ["a", "b", "c", "d", "e", "f", "g"]);
  assert.equal(queue.activeCount(), 0);
  assert.equal(queue.queuedCount(), 0);
});

test("a rejected task releases its slot and the next task starts", async () => {
  const queue = createRequestQueue(1);
  const boom = queue.enqueue("boom", async () => { throw new Error("network"); });
  const after = queue.enqueue("after", async () => "ok");
  await assert.rejects(boom, /network/);
  assert.equal(await after, "ok");
  assert.equal(queue.activeCount(), 0);
});

test("a synchronously throwing task releases its slot", async () => {
  const queue = createRequestQueue(1);
  const sync = queue.enqueue("sync", () => { throw new Error("sync boom"); });
  const next = queue.enqueue("next", () => "fine");
  await assert.rejects(sync, /sync boom/);
  assert.equal(await next, "fine");
  assert.equal(queue.activeCount(), 0);
});

test("duplicate clicks share one queued request", async () => {
  const queue = createRequestQueue(1);
  let calls = 0;
  let release;
  const first = queue.enqueue("a", async () => {
    calls++;
    await new Promise((resolve) => { release = resolve; });
    return "ok";
  }, { priority: "background" });
  const second = queue.enqueue("a", () => assert.fail("duplicate"), { priority: "manual" });
  assert.equal(first, second);
  await tick();
  release();
  assert.equal(await first, "ok");
  assert.equal(calls, 1);
});

test("a manual duplicate promotes its queued request ahead of taller priorities", async () => {
  const queue = createRequestQueue(1);
  const order = [];
  const blocker = controlled();
  const blocked = queue.enqueue("blocker", blocker.task);
  let duplicateRuns = 0;
  const low = queue.enqueue("low", async () => order.push("low"), { priority: "background" });
  const high = queue.enqueue("high", async () => order.push("high"));
  const duplicate = queue.enqueue("low", async () => { duplicateRuns++; order.push("dup"); }, { priority: "manual" });
  assert.equal(duplicate, low);
  blocker.release();
  await blocked;
  await Promise.all([low, high, duplicate]);
  assert.deepEqual(order, ["low", "high"]);
  assert.equal(duplicateRuns, 0);
});

test("equal priorities run in FIFO order even when enqueued while busy", async () => {
  const queue = createRequestQueue(1);
  const order = [];
  const blocker = controlled();
  const first = queue.enqueue("first", blocker.task);
  const second = queue.enqueue("second", async () => order.push("second"));
  const third = queue.enqueue("third", async () => order.push("third"));
  const fourth = queue.enqueue("fourth", async () => order.push("fourth"));
  blocker.release();
  await Promise.all([first, second, third, fourth]);
  assert.deepEqual(order, ["second", "third", "fourth"]);
});

test("cancelQueued removes a task that never runs and settles its promise", async () => {
  const queue = createRequestQueue(1);
  const blocker = controlled();
  let ran = false;
  const first = queue.enqueue("first", blocker.task);
  const victim = queue.enqueue("victim", () => { ran = true; return "should not run"; });
  assert.equal(queue.cancelQueued("victim"), true);
  assert.equal(await victim, CANCELLED);
  assert.equal(queue.queuedCount(), 0);
  blocker.release();
  await first;
  await tick();
  assert.equal(ran, false);
  assert.equal(queue.cancelQueued("victim"), false);
  assert.equal(queue.cancelQueued("unknown"), false);
});

test("cancelQueued refuses to pretend-cancel a running task", async () => {
  const queue = createRequestQueue(1);
  const running = controlled();
  const job = queue.enqueue("running", running.task);
  await tick();
  assert.equal(queue.cancelQueued("running"), false);
  running.release("done");
  assert.equal(await job, "done");
});

test("a task that expires before it starts settles without running its work", async () => {
  const queue = createRequestQueue(1);
  const blocker = controlled();
  let ran = false;
  const first = queue.enqueue("first", blocker.task);
  const stale = queue.enqueue("stale", () => {
    ran = true;
    return "work";
  });
  queue.cancelQueued("stale");
  blocker.release();
  await first;
  assert.equal(await stale, CANCELLED);
  assert.equal(ran, false);
  assert.equal(queue.activeCount(), 0);
  assert.equal(queue.queuedCount(), 0);
});

test("limit of one serializes everything", async () => {
  const queue = createRequestQueue(1);
  let active = 0;
  let peak = 0;
  const jobs = Array.from({ length: 5 }, (_, i) => queue.enqueue(i, async () => {
    active++;
    peak = Math.max(peak, active);
    await tick();
    active--;
    return i;
  }));
  assert.deepEqual(await Promise.all(jobs), [0, 1, 2, 3, 4]);
  assert.equal(peak, 1);
});
