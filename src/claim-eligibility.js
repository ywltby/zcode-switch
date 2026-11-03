// Auto-claim admission: one place decides whether an already-queued plan may be
// submitted. The GUI scans concurrently and enqueues; the consumer must still check
// the plan's window, the persisted "already claimed" mark and unresolved results,
// because a scan result can go stale while it waits for a slot.

export const planIdOf = (plan) => plan?.plan_id || plan?.planId || "";

const num = (value) => (Number.isFinite(value) ? value : null);

/**
 * Effective/start/end timestamps of a plan (ms). `starts_at` is the campaign start;
 * `effective_at` only stands in when the campaign start is missing, matching the
 * activity list's "start → end" semantics.
 */
export function planWindow(plan) {
  const campaign = plan?.campaign || null;
  const startAt = num(plan?.starts_at) ?? num(campaign?.starts_at) ?? num(campaign?.startsAt)
    ?? num(plan?.effective_at);
  const endsAt = num(plan?.ends_at) ?? num(campaign?.ends_at) ?? num(campaign?.endsAt);
  return { startAt, endsAt };
}

/**
 * @param {object} args
 * @param {object} args.plan            queued claim plan
 * @param {number} args.now             clock injection for tests
 * @param {(planId: string) => boolean} args.isMarked  persisted per-account claim mark
 * @param {boolean} args.hasUnknown     an unresolved submission exists for this account
 * @param {number} args.holdUntil       per-(identity|plan) hold from a terminal/challenge result
 * @returns {boolean} whether the consumer may submit this plan now
 */
export function autoEligible({
  plan,
  now = Date.now(),
  isMarked = () => false,
  hasUnknown = false,
  hasLegacy = false,
  holdUntil = 0,
}) {
  const planId = planIdOf(plan);
  if (!planId) return false;
  // 无身份的旧历史无法归属：在用户确认/清除前不允许自动领取。
  if (hasLegacy) return false;
  if (hasUnknown) return false;
  if (holdUntil > now) return false;
  if (isMarked(planId)) return false;
  const { startAt, endsAt } = planWindow(plan);
  if (startAt !== null && startAt > now) return false;
  if (endsAt !== null && endsAt <= now) return false;
  return true;
}

/** 领取标记/目录的身份键：优先用后端返回的作用域（渠道:主体 的哈希，与 claim 会话
 *  定义一致）；后端未提供时才退回「渠道:主体」，绝不用固定 live 或裸 user_id。 */
export function claimIdentityKey(id, { live = false, identity = null, liveHash = "" } = {}) {
  const info = identity || {};
  const scope = info.scope;
  if (scope) return `${live ? "live" : "acct"}:${scope}`;
  const channel = info.provider || "unknown";
  const who = info.user_id || info.userId || info.id || info.email || info.username || info.name;
  // 无后端作用域时：有主体就用「渠道:主体」，否则退回账号 id / live 快照哈希，
  // 绝不把两个账号或两个登录合并成同一个键。
  const fallback = who ? `${channel}:${who}` : live ? liveHash || "unknown" : id;
  return `${live ? "live" : "acct"}:${fallback}`;
}

export const claimMarkKey = (id, planId, context) => `${claimIdentityKey(id, context)}|${planId}`;
