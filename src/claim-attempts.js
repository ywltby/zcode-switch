// Claim attempt bookkeeping: results are accepted per claimId and validated
// against the account generation that existed when the attempt started.
// Waiters are only consumers; losing one never discards a result.

export const ATTEMPT_TTL_MS = 10 * 60 * 1000;
export const ATTEMPT_MAX = 100;

export function createClaimAttempts() {
  return { attempts: new Map(), order: [], accounts: {}, epochSeq: 0 };
}

export function syncClaimAccounts(model, accounts) {
  const next = {};
  for (const account of accounts || []) {
    const old = model.accounts[account.id];
    next[account.id] = old && old.revision === account.quota_revision
      ? old
      : { revision: account.quota_revision, epoch: 0 };
  }
  let seq = model.epochSeq;
  for (const id of Object.keys(next)) {
    if (!next[id].epoch) seq += 1, next[id] = { ...next[id], epoch: seq };
  }
  return { ...model, accounts: next, epochSeq: seq };
}

export function registerAttempt(model, { claimId, accountId, revision, planId, mode = "manual" }) {
  const existing = model.attempts.get(claimId);
  if (existing) return existing;
  const account = model.accounts[accountId];
  const attempt = {
    claimId,
    accountId,
    revision,
    planId,
    mode,
    epoch: account?.epoch ?? 0,
    handled: false,
    delivered: false,
    result: null,
    waiter: null,
    done: false,
    startedAt: Date.now(),
  };
  model.attempts.set(claimId, attempt);
  model.order.push(claimId);
  pruneAttempts(model);
  return attempt;
}

export function waitForAttempt(model, claimId, { timeoutMs = 90_000 } = {}) {
  const attempt = model.attempts.get(claimId);
  if (!attempt) return Promise.resolve(null);
  if (attempt.handled) {
    attempt.delivered = true;
    return Promise.resolve(attempt.result);
  }
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      attempt.waiter = null;
      attempt.done = true;
      resolve(null);
    }, timeoutMs);
    attempt.waiter = (payload) => {
      clearTimeout(timer);
      attempt.waiter = null;
      attempt.done = true;
      attempt.delivered = true;
      resolve(payload);
    };
  });
}

/**
 * Records a final result once. Never completes the waiter: callers refresh the
 * matching row first, then call finishWaiter.
 */
export function settleAttempt(model, claimId, payload) {
  const attempt = model.attempts.get(claimId);
  if (!attempt) return { status: "unknown_claim", refresh: false, attempt: null };
  if (attempt.handled) return { status: "duplicate", refresh: false, attempt };
  attempt.handled = true;
  attempt.result = payload || null;
  const account = model.accounts[attempt.accountId];
  const refresh = Boolean(account)
    && account.epoch === attempt.epoch
    && account.revision === attempt.revision;
  attempt.needsRefresh = refresh;
  return { status: "accepted", refresh, attempt };
}

export function finishWaiter(model, claimId, payload = null) {
  const attempt = model.attempts.get(claimId);
  if (!attempt?.waiter) return false;
  const waiter = attempt.waiter;
  attempt.waiter = null;
  attempt.done = true;
  attempt.delivered = true;
  waiter(payload ?? attempt.result);
  return true;
}

/**
 * Backend-side cancellation: the session is closed without a claim outcome.
 * Recorded like any other result so a late event cannot deliver it twice.
 */
export function cancelAttempt(model, claimId) {
  const attempt = model.attempts.get(claimId);
  if (!attempt) return { status: "unknown_claim", refresh: false, attempt: null, payload: null };
  const payload = {
    claimId,
    accountId: attempt.accountId,
    revision: attempt.revision,
    cancelled: true,
    ok: false,
  };
  const settled = settleAttempt(model, claimId, payload);
  return { ...settled, payload };
}

export function closeWaiter(model, claimId) {
  const attempt = model.attempts.get(claimId);
  if (!attempt) return false;
  attempt.done = true;
  if (!attempt.waiter) return false;
  const waiter = attempt.waiter;
  attempt.waiter = null;
  waiter(null);
  return true;
}

export function pendingAttemptFor(model, accountId) {
  for (const attempt of model.attempts.values()) {
    if (attempt.accountId === accountId && !attempt.handled && attempt.mode !== "manual") return attempt;
  }
  return null;
}

export function pruneAttempts(model, now = Date.now()) {
  const finished = model.order.filter((id) => model.attempts.get(id)?.handled);
  const drop = new Set(finished.filter((id) => now - model.attempts.get(id).startedAt > ATTEMPT_TTL_MS));
  let remaining = finished.length - drop.size;
  for (const id of finished) {
    if (remaining <= ATTEMPT_MAX) break;
    if (drop.has(id)) continue;
    drop.add(id);
    remaining -= 1;
  }
  if (!drop.size) return false;
  for (const id of drop) model.attempts.delete(id);
  model.order = model.order.filter((id) => !drop.has(id));
  return true;
}
