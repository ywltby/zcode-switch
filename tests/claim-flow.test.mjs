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
