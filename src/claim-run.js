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
