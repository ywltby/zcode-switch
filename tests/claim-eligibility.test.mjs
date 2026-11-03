import test from "node:test";
import assert from "node:assert/strict";

import { autoEligible, claimIdentityKey, claimMarkKey, planIdOf, planWindow } from "../src/claim-eligibility.js";

const NOW = 1_700_000_000_000;
const markNone = () => false;

test("a plan already claimed by this account is never submitted again", () => {
  const plan = { plan_id: "p1", starts_at: NOW - 1000, ends_at: NOW + 60_000 };
  assert.equal(autoEligible({ plan, now: NOW, isMarked: markNone }), true);
  assert.equal(
    autoEligible({ plan, now: NOW, isMarked: (planId) => planId === "p1" }),
    false,
    "持久已领取标记必须挡住自动提交",
  );
});

test("a future plan is not submitted before its start", () => {
  const future = { plan_id: "p1", starts_at: NOW + 60_000, ends_at: NOW + 120_000 };
  assert.equal(autoEligible({ plan: future, now: NOW, isMarked: markNone }), false);
  assert.equal(autoEligible({ plan: future, now: NOW + 60_000, isMarked: markNone }), true);
});

test("an expired plan is not submitted", () => {
  const expired = { plan_id: "p1", starts_at: NOW - 120_000, ends_at: NOW - 1000 };
  assert.equal(autoEligible({ plan: expired, now: NOW, isMarked: markNone }), false);
  assert.equal(
    autoEligible({ plan: { ...expired, ends_at: NOW }, now: NOW, isMarked: markNone }),
    false,
    "刚好到期也算过期",
  );
});

test("an unresolved submission blocks every auto path for this account", () => {
  const plan = { plan_id: "p1" };
  assert.equal(autoEligible({ plan, now: NOW, isMarked: markNone, hasUnknown: true }), false);
});

test("campaign window is used when plan-level timestamps are absent", () => {
  const campaignPlan = {
    plan_id: "p1",
    campaign: { starts_at: NOW + 10_000, ends_at: NOW + 20_000 },
  };
  assert.equal(autoEligible({ plan: campaignPlan, now: NOW, isMarked: markNone }), false);
  assert.equal(autoEligible({ plan: campaignPlan, now: NOW + 15_000, isMarked: markNone }), true);
});

test("effective_at only stands in for a missing campaign start", () => {
  const noStart = { plan_id: "p1", effective_at: NOW + 5_000, ends_at: NOW + 60_000 };
  assert.equal(autoEligible({ plan: noStart, now: NOW, isMarked: markNone }), false);
  const withStart = { ...noStart, starts_at: NOW - 5_000 };
  assert.equal(
    autoEligible({ plan: withStart, now: NOW, isMarked: markNone }),
    true,
    "starts_at 优先于 effective_at（权益生效时间不是活动开始）",
  );
});

test("a plan without any window is eligible once nothing else blocks it", () => {
  assert.equal(autoEligible({ plan: { plan_id: "p1" }, now: NOW, isMarked: markNone }), true);
  assert.equal(autoEligible({ plan: { name: "no id" }, now: NOW, isMarked: markNone }), false);
});

test("marks are account/plan scoped by the caller", () => {
  const plan = { plan_id: "p1" };
  const marks = new Map([["account-b|p1", true]]);
  const marked = (accountId) => (planId) => marks.get(`${accountId}|${planId}`) === true;
  assert.equal(autoEligible({ plan, now: NOW, isMarked: marked("account-a") }), true);
  assert.equal(autoEligible({ plan, now: NOW, isMarked: marked("account-b") }), false);
});

test("helpers expose the plan id and window used by the activity list", () => {
  assert.equal(planIdOf({ planId: "camel" }), "camel");
  assert.deepEqual(planWindow({ starts_at: 1, campaign: { ends_at: 2 } }), {
    startAt: 1,
    endsAt: 2,
  });
});

test("claim marks are keyed by the backend scope, not user_id or a fixed live", () => {
  const live = (identity, hash = "h1") => ({ live: true, identity, liveHash: hash });
  // 后端作用域优先：渠道与主体由后端定义，前端不自行拼接
  const scoped = claimIdentityKey("live", live({ scope: "scope-1", user_id: "u1" }));
  assert.equal(scoped, "live:scope-1");
  assert.notEqual(
    scoped,
    claimIdentityKey("live", live({ scope: "scope-2", user_id: "u1" })),
    "同一 user_id 不同渠道/作用域必须区分",
  );
  // 无作用域时退回「渠道:主体」，仍然包含渠道
  assert.equal(
    claimIdentityKey("live", live({ provider: "zai", user_id: "u1" })),
    "live:zai:u1",
  );
  assert.notEqual(
    claimIdentityKey("live", live({ provider: "zai", user_id: "u1" })),
    claimIdentityKey("live", live({ provider: "bigmodel", user_id: "u1" })),
    "不同渠道不得合并",
  );
  assert.equal(claimIdentityKey("live", live(null, "hash-only")), "live:hash-only");
  // 存储账号同样按作用域分区：保存账号被换成另一登录后不继承旧标记
  assert.equal(
    claimIdentityKey("account-a", { live: false, identity: { scope: "scope-a" } }),
    "acct:scope-a",
  );
  assert.notEqual(
    claimIdentityKey("account-a", { live: false, identity: { scope: "scope-a" } }),
    claimIdentityKey("account-a", { live: false, identity: { scope: "scope-b" } }),
  );
  assert.equal(
    claimMarkKey("live", "p1", live({ scope: "scope-1" })),
    "live:scope-1|p1",
    "标记必须按身份+套餐隔离",
  );
});

test("the caller supplies the context so one helper serves both row kinds", () => {
  assert.equal(claimMarkKey("a1", "p1", { live: false }), "acct:a1|p1");
  assert.notEqual(
    claimMarkKey("a1", "p1", { live: false }),
    claimMarkKey("a1", "p2", { live: false }),
  );
});

test("a held plan is not submitted by any automatic path", () => {
  const now = 1_700_000_000_000;
  const plan = { plan_id: "p1" };
  // 终态：holdUntil = Infinity
  assert.equal(
    autoEligible({ plan, now, isMarked: markNone, holdUntil: Infinity }),
    false,
    "终态必须挡住所有自动路径",
  );
  // 1005 / 3007 / challenge / unknown：60 分钟内的 hold
  assert.equal(
    autoEligible({ plan, now, isMarked: markNone, holdUntil: now + 60_000 }),
    false,
    "冷却期内不得提交",
  );
  assert.equal(
    autoEligible({ plan, now: now + 61_000, isMarked: markNone, holdUntil: now + 60_000 }),
    true,
    "冷却结束后恢复可提交",
  );
});

test("hold, claim mark and window all participate in one decision", () => {
  const now = 1_700_000_000_000;
  const future = { plan_id: "p1", starts_at: now + 10_000 };
  assert.equal(
    autoEligible({ plan: future, now, isMarked: markNone, holdUntil: now + 1 }),
    false,
    "时间窗与 hold 同时生效，都不能绕过",
  );
});

test("unattributed legacy history blocks automatic admission", () => {
  const plan = { plan_id: "p1" };
  assert.equal(autoEligible({ plan, now: NOW, hasLegacy: true }), false);
  assert.equal(autoEligible({ plan, now: NOW }), true);
});
