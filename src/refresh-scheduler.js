// Quota refresh orchestration: bounded concurrency, per-account cadence, and
// stale-result rejection. Rendering and IPC details stay with the caller.

import { CANCELLED, createRequestQueue } from "./request-queue.js";
import {
  beginQuotaRequest,
  createQuotaState,
  releaseQuotaRequest,
  settleQuotaRequest,
  syncQuotaAccounts,
} from "./quota-state.js";

export const SWEEP_PERIOD_MS = 5 * 60 * 1000;
export const SWEEP_JITTER = 0.2;

const quotaKey = (ticket) => JSON.stringify(["quota", ticket.id, ticket.revision, ticket.sequence]);

export function createQuotaScheduler({
  invoke,
  queue: sharedQueue = null,
  now = () => Date.now(),
  random = () => Math.random(),
  limit = 3,
  periodMs = SWEEP_PERIOD_MS,
  jitter = SWEEP_JITTER,
  classifyData = () => null,
  errorText = (error) => String(error),
  onChange = () => {},
}) {
  const queue = sharedQueue || createRequestQueue(limit);
  let model = createQuotaState();
  const due = new Map();
  const stamps = new Map();
  const inFlight = new Map();
  let live = [];
  let liveSignature = "";
  let stampSeq = 0;

  function emit() {
    onChange(model);
  }

  function setModel(next) {
    if (next !== model) {
      model = next;
      emit();
    }
  }

  function setDue(id, at) {
    due.set(id, at);
    stamps.set(id, ++stampSeq);
  }

  function scheduleNext(id, base = now()) {
    if (!live.some((a) => a.id === id)) {
      due.delete(id);
      stamps.delete(id);
      return;
    }
    const factor = 1 + (random() * 2 - 1) * jitter;
    setDue(id, base + Math.round(periodMs * factor));
  }

  function applyAccounts(accounts) {
    const next = accounts || [];
    const signature = next.map((a) => `${a.id}\u0000${a.quota_revision}`).join("\u0001");
    if (signature === liveSignature) {
      live = next;
      return;
    }
    liveSignature = signature;
    live = next;
    const seen = new Set(live.map((a) => a.id));
    for (const id of [...due.keys()]) if (!seen.has(id)) { due.delete(id); stamps.delete(id); }
    for (const id of [...inFlight.keys()]) if (!seen.has(id)) cancel(id);
    setModel(syncQuotaAccounts(model, live));
    for (const account of live) {
      if (!due.has(account.id)) {
        setDue(account.id, now());
        continue;
      }
      const entry = model.entries[account.id];
      const pending = inFlight.get(account.id);
      if (pending && pending.ticket.revision !== account.quota_revision) {
        cancel(account.id);
        setDue(account.id, now());
      } else if (entry && entry.revision !== account.quota_revision) {
        setDue(account.id, now());
      }
    }
  }

  function cancel(id) {
    const pending = inFlight.get(id);
    if (!pending) return false;
    inFlight.delete(id);
    const cancelled = queue.cancelQueued(pending.key);
    setModel(releaseQuotaRequest(model, pending.ticket));
    return cancelled;
  }

  function invalidate(id) {
    cancel(id);
    due.delete(id);
    stamps.delete(id);
    const entries = { ...model.entries };
    delete entries[id];
    setModel({ ...model, entries });
  }

  function isCurrent(ticket) {
    const entry = model.entries[ticket.id];
    return Boolean(entry?.busy && entry.revision === ticket.revision && entry.sequence === ticket.sequence);
  }

  async function run(ticket) {
    // Re-check ownership after leaving the queue: the row may have been deleted,
    // re-added, or credential-updated while this task waited for a slot.
    if (!isCurrent(ticket)) return CANCELLED;
    let data;
    let error = null;
    try {
      data = await invoke("get_account_quota", { id: ticket.id, revision: ticket.revision });
      if (data?.account_id !== ticket.id || data?.revision !== ticket.revision) throw new Error("scope_changed");
      error = classifyData(data) || null;
    } catch (e) {
      data = undefined;
      error = errorText(e);
    }
    const settled = settleQuotaRequest(model, ticket, error ? { error } : { data });
    const accepted = settled !== model;
    setModel(settled);
    return accepted && !error ? data : undefined;
  }

  function request(id, { priority = "manual" } = {}) {
    const existing = inFlight.get(id);
    if (existing) {
      if (priority === "manual") queue.promote(existing.key);
      return existing.promise;
    }
    const begun = beginQuotaRequest(model, id);
    if (!begun.ticket) return Promise.resolve(CANCELLED);
    setModel(begun.model);
    const ticket = begun.ticket;
    const key = quotaKey(ticket);
    const startedStamp = stamps.get(id);
    const promise = queue.enqueue(key, () => run(ticket), { priority }).then((result) => {
      const current = inFlight.get(id);
      if (current?.key === key) inFlight.delete(id);
      if (stamps.get(id) === startedStamp) scheduleNext(id);
      emit();
      return result;
    });
    inFlight.set(id, { key, ticket, promise });
    emit();
    return promise;
  }

  function sweep({ isBlocked = () => false } = {}) {
    const at = now();
    const ids = live.filter((a) => (due.get(a.id) ?? Infinity) <= at && !inFlight.has(a.id) && !isBlocked(a.id));
    for (const account of ids) request(account.id, { priority: "background" });
    return ids.length;
  }

  return {
    applyAccounts,
    request,
    sweep,
    invalidate,
    scheduleNext,
    poke(id) { setDue(id, now()); },
    dueAt: (id) => due.get(id),
    entries: () => model.entries,
    busy: (id) => Boolean(model.entries[id]?.busy),
    pending: () => inFlight.size,
    queueStats: () => ({ active: queue.activeCount(), queued: queue.queuedCount(), limit: queue.limit }),
  };
}
