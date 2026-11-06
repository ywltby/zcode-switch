import test from "node:test";
import assert from "node:assert/strict";

import { createClaimFlow } from "../src/claim-flow.js";
import { createRequestQueue } from "../src/request-queue.js";

const makeFlow = (handler) => createClaimFlow({
  invoke: handler,
  queue: createRequestQueue(3),
  errorText: (error) => String(error),
  isLive: (id) => id === "live",
});

test("live eligibility adopts the backend revision and claims bind to it", async () => {
  const calls = [];
  const flow = makeFlow(async (command, args) => {
    calls.push({ command, args });
    if (command === "claim_preview") {
      return { accountId: "live", revision: "backend-1", plans: [{ plan_id: "p1", name: "synthetic" }] };
    }
    return {};
  });
  flow.applyAccounts([], [{ id: "live", revision: "hash-1" }]);
  const outcome = await flow.preview("live");
  assert.equal(outcome.status, "ok");
  assert.equal(calls.length, 1, "live preview must be a single request");
  assert.deepEqual(Object.keys(calls[0].args), ["id"], "live preview must not send a stored revision");
  assert.equal(flow.entries().live.revision, "backend-1", "the row binds to the backend revision");
  assert.equal(flow.entries().live.plans.length, 1);
});

test("a stale live result cannot land after the login changed", async () => {
  let release;
  const flow = makeFlow(() => new Promise((resolve) => { release = resolve; }));
  flow.applyAccounts([], [{ id: "live", revision: "hash-1" }]);
  const pending = flow.preview("live");
  // 登录在请求返回前被替换
  flow.applyAccounts([], [{ id: "live", revision: "hash-2" }]);
  release({ accountId: "live", revision: "backend-1", plans: [{ plan_id: "p1", name: "synthetic" }] });
  const outcome = await pending;
  assert.notEqual(outcome.status, "ok");
  assert.equal(flow.entries().live.plans.length, 0, "旧代次的资格不得落到新登录");
  assert.equal(flow.entries().live.revision, "hash-2");
});

test("stored accounts still reject a mismatched revision from the backend", async () => {
  const flow = makeFlow(async () => ({
    accountId: "a",
    revision: "r2",
    plans: [{ plan_id: "p1", name: "synthetic" }],
  }));
  flow.applyAccounts([{ id: "a", quota_revision: "r1" }], []);
  const outcome = await flow.preview("a");
  assert.equal(outcome.status, "skipped");
  assert.equal(flow.entries().a.plans.length, 0);
});

test("a quiet preview updates the row without publishing a claim result", async () => {
  const published = [];
  const quietSeen = [];
  const flow = createClaimFlow({
    invoke: async () => ({ accountId: "a", revision: "r1", plans: [{ plan_id: "p1", name: "synthetic" }] }),
    queue: createRequestQueue(3),
    errorText: (error) => String(error),
    onResult: (result) => published.push(result),
    onQuietResult: (result) => quietSeen.push(result),
  });
  flow.applyAccounts([{ id: "a", quota_revision: "r1" }], []);

  const quiet = await flow.preview("a", { quiet: true });
  assert.equal(quiet.status, "ok");
  assert.equal(flow.entries().a.plans.length, 1, "the row still shows the fresh result");
  assert.deepEqual(published, [], "a quiet read must never reach the claim queue");
  assert.equal(quietSeen.length, 1, "a quiet read still reports its own snapshot binding");

  // 普通预览仍需照旧发布：这是自动领取的唯一来源。
  await flow.preview("a");
  assert.equal(published.length, 1);
  assert.equal(quietSeen.length, 1);
});

test("a quiet retry after scope_changed stays quiet", async () => {
  const published = [];
  const quietSeen = [];
  let attempt = 0;
  const flow = createClaimFlow({
    invoke: async () => {
      attempt += 1;
      if (attempt === 1) throw new Error('{"code":"scope_changed","notStarted":true}');
      return { accountId: "a", revision: "r1", plans: [{ plan_id: "p1", name: "synthetic" }] };
    },
    queue: createRequestQueue(3),
    errorText: (error) => String(error),
    onResult: (result) => published.push(result),
    onQuietResult: (result) => quietSeen.push(result),
  });
  flow.applyAccounts([{ id: "a", quota_revision: "r1" }], []);

  const outcome = await flow.preview("a", { quiet: true });
  assert.equal(attempt, 2, "scope_changed is retried once");
  assert.equal(outcome.status, "ok");
  assert.deepEqual(published, [], "the retried read must never publish a claim result");
  assert.equal(quietSeen.length, 1);
});

test("a claim stuck behind a full shared queue still times out and frees the row", async () => {
  const queue = createRequestQueue(1);
  // 占满唯一槽位：一个永不结算的任务，等价于额度请求卡死。
  queue.enqueue("hold", () => new Promise(() => {}));
  const flow = createClaimFlow({
    invoke: async () => { throw new Error("must not run while queued"); },
    queue,
    errorText: (error) => String(error),
    timeoutMs: 25,
    timeoutText: () => "claim timed out",
  });
  flow.applyAccounts([{ id: "a", quota_revision: "r1" }], []);

  const outcome = await flow.preview("a");
  assert.equal(outcome.status, "failed");
  assert.equal(outcome.error, "claim timed out");
  assert.equal(flow.entries().a.busy, false, "a queued request must not leave the row busy");
  assert.equal(queue.queuedCount(), 0, "the queued request is dropped, not left to run later");
});
