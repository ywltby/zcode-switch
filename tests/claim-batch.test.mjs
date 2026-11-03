import test from "node:test";
import assert from "node:assert/strict";

import { batchAdmission } from "../src/claim-batch.js";
import { claimHoldUntil } from "../src/claim-run.js";

const NOW = 1_700_000_000_000;
const plan = { plan_id: "p1", starts_at: NOW - 60_000, ends_at: NOW + 3_600_000 };
const account = { id: "a1", quota_revision: "r1" };
const base = () => ({
  plan,
  expected: { revision: "r1" },
  account,
  live: null,
  currentPlans: [plan],
  now: NOW,
  isMarked: () => false,
  hasUnknown: false,
  holdUntil: 0,
  cooldownUntil: 0,
});

test("a batch item is admitted only while its generation and eligibility still match", () => {
  assert.equal(batchAdmission(base()).ok, true);
  assert.deepEqual(
    batchAdmission({ ...base(), account: { id: "a1", quota_revision: "r2" } }),
    { ok: false, reason: "scope_changed" },
    "账号代次变化后不得提交",
  );
  assert.deepEqual(
    batchAdmission({ ...base(), currentPlans: [] }),
    { ok: false, reason: "not_eligible" },
    "资格已被撤销后不得提交",
  );
  assert.deepEqual(
    batchAdmission({ ...base(), currentPlans: [{ plan_id: "other" }] }),
    { ok: false, reason: "not_eligible" },
  );
});

test("holds, marks, unresolved results and cooldowns all block a batch item", () => {
  assert.deepEqual(batchAdmission({ ...base(), holdUntil: Infinity }), { ok: false, reason: "hold" });
  assert.deepEqual(
    batchAdmission({ ...base(), holdUntil: claimHoldUntil({ code: 1003, terminal: true }, NOW) }),
    { ok: false, reason: "hold" },
    "队列里领到的终态必须挡到点批次",
  );
  assert.deepEqual(batchAdmission({ ...base(), hasUnknown: true }), { ok: false, reason: "unknown" });
  assert.deepEqual(
    batchAdmission({ ...base(), isMarked: (planId) => planId === "p1" }),
    { ok: false, reason: "marked" },
  );
  assert.deepEqual(
    batchAdmission({ ...base(), cooldownUntil: NOW + 60_000 }),
    { ok: false, reason: "cooldown" },
  );
});

test("the live row is checked by snapshot hash, not by stored revision", () => {
  const liveBase = {
    ...base(),
    account: { id: "live", quota_revision: "unused" },
    expected: { revision: "live", liveHash: "hash-1" },
    live: { hash: "hash-1" },
  };
  assert.equal(batchAdmission(liveBase).ok, true);
  assert.deepEqual(
    batchAdmission({ ...liveBase, live: { hash: "hash-2" } }),
    { ok: false, reason: "scope_changed" },
    "换登录后到点批次不得提交旧 plan",
  );
});

test("the batch uses the latest plan window, not only the old plan id", () => {
  const stale = { plan_id: "p1" };
  const future = { plan_id: "p1", starts_at: NOW + 60_000, ends_at: NOW + 120_000 };
  assert.equal(
    batchAdmission({ ...base(), plan: stale, currentPlans: [future] }).ok,
    false,
    "最新套餐还没开始，不能因为旧 ID 仍在列表里就提交",
  );
  const expired = { plan_id: "p1", starts_at: NOW - 120_000, ends_at: NOW - 1_000 };
  assert.equal(
    batchAdmission({ ...base(), plan: stale, currentPlans: [expired] }).ok,
    false,
    "最新套餐已过期也不能提交",
  );
  const live = { plan_id: "p1", starts_at: NOW - 10_000, ends_at: NOW + 60_000 };
  assert.equal(batchAdmission({ ...base(), plan: stale, currentPlans: [live] }).ok, true);
});

test("unattributed legacy history blocks the batch until the user decides", () => {
  assert.deepEqual(batchAdmission({ ...base(), hasLegacy: true }), { ok: false, reason: "legacy" });
});
