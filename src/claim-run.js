// Claim timeout classification: the frontend wait ends, the backend session does not.
// Injected IPC keeps this testable without duplicating the orchestration in mocks.

export const TERMINAL_STATUSES = ["succeeded", "failed", "unknown", "cancelled"];
export const defaultSleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export function payloadFromStatus(status) {
  const result = status?.result && typeof status.result === "object" ? status.result : {};
  return {
    ...result,
    claimId: status?.claimId,
    accountId: status?.accountId,
    accountName: status?.accountName || result.accountName,
    revision: status?.revision,
    planId: status?.planId || result.planId,
    planName: status?.planName || result.planName,
    ok: status?.status === "succeeded" && result.ok !== false,
    unknown: status?.status === "unknown",
    cancelled: status?.status === "cancelled",
  };
}

export function claimOutcomeOf(payload) {
  // 没有结果载荷（准备失败、传输中断、超时无结果）一律按失败处理：
  // 当成成功会跳过标记、hold 与冷却，导致自动路径立即重复启动。
  if (!payload || typeof payload !== "object" || Object.keys(payload).length === 0) {
    return { status: "failed", payload: payload || null };
  }
  if (payload?.unknown) return { status: "unknown", payload };
  if (payload?.cancelled) return { status: "cancelled", payload };
  if (payload?.ok === false) return { status: "failed", payload };
  return { status: "ok", payload };
}

// A timed-out UI waiter must not strand the serial queue when the backend's
// completion event is lost. Only a stored terminal result can release it.
export async function reconcilePendingClaims({ attempts, invoke, handleResult }) {
  for (const attempt of attempts) {
    if (attempt.handled || (!attempt.done && !attempt.confirming)) continue;
    let status;
    try {
      status = await invoke("claim_status", { claimId: attempt.claimId });
    } catch {
      continue;
    }
    if (!status || attempt.handled) continue;
    if (TERMINAL_STATUSES.includes(status.status)) handleResult(payloadFromStatus(status));
    else if (status.status === "submitting") attempt.confirming = true;
  }
}

export async function waitForBackendResult({
  invoke,
  claimId,
  mode,
  handleResult,
  sleep = defaultSleep,
  now = Date.now,
  autoWaitMs = 45_000,
  isAborted = () => false,
}) {
  const deadline = now() + (mode === "auto" ? autoWaitMs : 120_000);
  while (now() < deadline) {
    if (mode === "auto" && isAborted()) break;
    await sleep(1000);
    let status = null;
    try {
      status = await invoke("claim_status", { claimId });
    } catch {
      status = null;
    }
    if (!status) continue;
    if (TERMINAL_STATUSES.includes(status.status)) {
      const payload = payloadFromStatus(status);
      handleResult(payload);
      return claimOutcomeOf(payload);
    }
  }
  return { status: "unknown" };
}

/**
 * - completed: consume the stored result even if the event was lost
 * - not submitted yet: cancel the session and end this wait
 * - submitting: keep the backend mutex; manual waits stop, batch/auto wait it out
 */
export async function resolveClaimTimeout({
  invoke,
  claimId,
  mode,
  attempt,
  handleResult,
  onConfirming = () => {},
  onTimeout = () => {},
  sleep = defaultSleep,
  now = Date.now,
  autoWaitMs = 45_000,
  isAborted = () => false,
}) {
  if (!attempt) return { status: "skipped" };
  let status = null;
  try {
    status = await invoke("claim_status", { claimId });
  } catch {
    status = null;
  }
  if (!status) return { status: "unknown" };
  if (status.status === "preparing" || status.status === "verifying") {
    await invoke("claim_cancel", { claimId }).catch(() => {});
    onTimeout();
    return { status: "cancelled" };
  }
  if (status.status === "submitting") {
    if (mode === "manual") {
      onConfirming();
      return { status: "submitting" };
    }
    return waitForBackendResult({ invoke, claimId, mode, handleResult, sleep, now, autoWaitMs, isAborted });
  }
  const payload = payloadFromStatus(status);
  handleResult(payload);
  return claimOutcomeOf(payload);
}

/// (账号|套餐) 级暂停：终态永不重试，1005 等到上游时间，挑战/3007/未知统一 60 分钟。
export const CHALLENGE_HOLD_MS = 60 * 60 * 1000;

export function claimHoldUntil(payload, now = Date.now()) {
  const r = payload || {};
  if (r.terminal && r.code === 1005) {
    return Number.isFinite(r.nextAt) && r.nextAt > now ? r.nextAt : now + CHALLENGE_HOLD_MS;
  }
  if (r.terminal) return Infinity;
  if (r.challenge || r.code === 3007 || r.unknown) return now + CHALLENGE_HOLD_MS;
  return now + CHALLENGE_HOLD_MS;
}

/// 账号级自动冷却：1005 用上游时间，业务码失败 60 分钟，其余退回扫描间隔。
export function autoCooldownUntil(payload, now = Date.now(), retryMs = 10 * 60 * 1000) {
  const r = payload || {};
  if (r.code === 1005 && r.nextAt) return r.nextAt;
  if (Number.isFinite(r.code) && r.code >= 1000) return now + CHALLENGE_HOLD_MS;
  if (r.code === "interactive") return now + CHALLENGE_HOLD_MS;
  return now + retryMs;
}

/// 统一结算的副作用：成功清除该套餐的 hold；失败按结果写入 hold 与账号冷却。
/// 队列、定时扫描和到点领取都只读这份状态。
export function claimSettlement(payload, now = Date.now(), retryMs = 10 * 60 * 1000) {
  const outcome = claimOutcomeOf(payload);
  if (outcome.status === "ok") {
    return { status: "ok", holdUntil: 0, cooldownUntil: 0 };
  }
  return {
    status: outcome.status,
    holdUntil: claimHoldUntil(payload, now),
    cooldownUntil: outcome.status === "unknown"
      ? now + CHALLENGE_HOLD_MS
      : autoCooldownUntil(payload, now, retryMs),
  };
}
