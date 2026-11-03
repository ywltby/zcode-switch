// Batch admission for automatic claim rounds (scheduled scan and effective-time claims).
// A batch is built from a scan snapshot, but each item is submitted much later: the
// account generation, the current eligibility list, persisted marks, holds from earlier
// results and unresolved submissions must all be re-checked right before submitting.

import { autoEligible, planIdOf } from "./claim-eligibility.js";

/**
 * @param {object} args
 * @param {object} args.plan           plan the batch wants to submit
 * @param {{revision: string, liveHash?: string}} args.expected  generation captured when the batch was built
 * @param {{id: string, quota_revision: string}|null} args.account  current stored account row
 * @param {{hash: string}|null} args.live  current live row generation
 * @param {Array} args.currentPlans    plans currently considered claimable for that row
 * @param {number} args.now
 * @param {(planId: string) => boolean} args.isMarked
 * @param {boolean} args.hasUnknown
 * @param {number} args.holdUntil
 * @param {number} args.cooldownUntil
 * @returns {{ok: boolean, reason?: string}}
 */
export function batchAdmission({
  plan,
  expected = {},
  account = null,
  live = null,
  currentPlans = [],
  now = Date.now(),
  isMarked = () => false,
  hasUnknown = false,
  hasLegacy = false,
  holdUntil = 0,
  cooldownUntil = 0,
}) {
  if (!account) return { ok: false, reason: "account_missing" };
  // 账号代次：live 用登录快照哈希，存储账号用 revision。
  const generationMatches = live
    ? expected.liveHash === live.hash
    : expected.revision === account.quota_revision;
  if (!generationMatches) return { ok: false, reason: "scope_changed" };
  const planId = planIdOf(plan);
  if (!planId) return { ok: false, reason: "no_plan" };
  // 当前资格：批次快照里的套餐可能已被服务端撤销/领走。必须用最新套餐对象判断，
  // 只确认 plan_id 仍存在会让「时间窗已改到未来/已过期」的套餐被继续提交。
  const latest = (currentPlans || []).find((p) => planIdOf(p) === planId);
  if (!latest) return { ok: false, reason: "not_eligible" };
  if (hasLegacy) return { ok: false, reason: "legacy" };
  if (hasUnknown) return { ok: false, reason: "unknown" };
  if (holdUntil > now) return { ok: false, reason: "hold" };
  if (cooldownUntil > now) return { ok: false, reason: "cooldown" };
  if (isMarked(planId)) return { ok: false, reason: "marked" };
  // 统一的时间窗/待确认判定（与队列入口同一份规则），输入是最新套餐对象。
  if (!autoEligible({ plan: latest, now, isMarked, hasUnknown, hasLegacy, holdUntil })) {
    return { ok: false, reason: "ineligible" };
  }
  return { ok: true };
}
