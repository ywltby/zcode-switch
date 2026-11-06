// Claim eligibility refresh orchestration over the shared bounded queue.
// Preview and refresh results are accepted only for the current account ticket.

import { CANCELLED, createRequestQueue } from "./request-queue.js";
import {
  beginClaimRequest,
  createClaimState,
  invalidateClaimEntry,
  releaseClaimRequest,
  setClaimRevision,
  settleClaimRequest,
  syncClaimAccounts,
} from "./claim-state.js";

const claimKey = (ticket) => JSON.stringify(["claim", ticket.kind, ticket.id, ticket.revision, ticket.sequence]);

// The live row is not a stored account: the backend derives its own revision, so the
// frontend only needs a non-empty placeholder to own the request.
const LIVE_REVISION = "live";

export function parseScopeError(error) {
  const text = typeof error === "string" ? error : String(error);
  const start = text.indexOf("{");
  if (start < 0) return null;
  try {
    const parsed = JSON.parse(text.slice(start));
    if (parsed?.code !== "scope_changed") return null;
    return { notStarted: parsed.notStarted !== false, accountId: parsed.accountId, revision: parsed.revision };
  } catch {
    return null;
  }
}

export function createClaimFlow({
  invoke,
  queue = createRequestQueue(3),
  errorText = (error) => String(error),
  onChange = () => {},
  onResult = () => {},
  onQuietResult = () => {},
  refreshState = async () => {},
  isLive = () => false,
  liveHash = () => "",
  timeoutMs = 0,
  timeoutText = () => "request timeout",
}) {
  let model = createClaimState();
  let live = [];
  let liveSignature = "";
  const inFlight = new Map();

  function emit() {
    onChange(model);
  }

  function setModel(next) {
    if (next !== model) {
      model = next;
      emit();
    }
  }

  function current(ticket) {
    const entry = model.entries[ticket.id];
    return Boolean(entry?.busy && entry.revision === ticket.revision && entry.sequence === ticket.sequence);
  }

  function cancel(id) {
    const pending = inFlight.get(id);
    if (!pending) return;
    inFlight.delete(id);
    queue.cancelQueued(pending.key);
    setModel(releaseClaimRequest(model, pending.ticket));
  }

  // Watchdog on the whole queue wait, not just the request: the queue is shared with
  // quota reads, so a claim can sit behind three stuck requests long before invoke()
  // is ever called. Without this the row stays busy and auto scanning never resumes.
  function expire(ticket, key) {
    const pending = inFlight.get(ticket.id);
    if (pending?.key !== key) return;
    inFlight.delete(ticket.id);
    queue.cancelQueued(key);
    setModel(settleClaimRequest(model, ticket, { error: timeoutText() }));
  }

  function guard(ticket, key, started) {
    if (!(timeoutMs > 0)) return started;
    let timer = null;
    const timeout = new Promise((resolve) => {
      timer = setTimeout(() => {
        // Settle the race first: cancelling the queued job resolves its own promise,
        // which would otherwise win the race and report a plain cancellation.
        resolve({ status: "failed", error: timeoutText() });
        expire(ticket, key);
      }, timeoutMs);
    });
    return Promise.race([started, timeout]).finally(() => {
      if (timer) clearTimeout(timer);
    });
  }

  function applyAccounts(accounts, extraIds = []) {
    // Extra rows (e.g. the live login) may carry a real generation key: callers pass
    // `{ id, revision }` so a replaced login changes the signature and invalidates
    // tickets from the previous one. Plain strings keep the placeholder revision.
    const extra = (extraIds || []).map((entry) => typeof entry === "string"
      ? { id: entry, quota_revision: LIVE_REVISION }
      : { id: entry.id, quota_revision: entry.revision || LIVE_REVISION });
    const next = [...(accounts || []), ...extra];
    const signature = next.map((a) => `${a.id}\u0000${a.quota_revision}`).join("\u0001");
    if (signature === liveSignature) {
      live = next;
      return;
    }
    liveSignature = signature;
    live = next;
    for (const [id, pending] of [...inFlight]) {
      const account = live.find((a) => a.id === id);
      if (!account || account.quota_revision !== pending.ticket.revision) cancel(id);
    }
    setModel(syncClaimAccounts(model, live));
  }

  async function run(ticket) {
    if (!current(ticket)) return { status: "skipped" };
    const command = ticket.kind === "refresh" ? "claim_refresh" : "claim_preview";
    try {
      const liveRow = isLive(ticket.id);
      // 界面当前展示的 live 快照：结果若属于另一个登录，直接丢弃（不 settle、不采纳 revision）。
      const expectedLiveHash = liveRow ? liveHash() : "";
      const response = await invoke(command, liveRow
        ? { id: ticket.id }
        : { id: ticket.id, revision: ticket.revision });
      if (response?.accountId !== ticket.id) return { status: "skipped" };
      if (liveRow && response.liveHash && expectedLiveHash && response.liveHash !== expectedLiveHash) {
        return { status: "skipped" };
      }
      // The live row's revision is a placeholder; the backend owns the real one.
      if (!liveRow && response.revision && response.revision !== ticket.revision) return { status: "skipped" };
      const settled = settleClaimRequest(model, ticket, {
        plans: response.plans,
        activationError: response.activationError,
      });
      const accepted = settled !== model;
      // The live row binds to the backend's own revision: the first eligibility read is
      // what makes a later claim reject a login that changed in between.
      const adopted = accepted && liveRow && typeof response.revision === "string"
        ? setClaimRevision(settled, ticket.id, response.revision)
        : settled;
      setModel(adopted);
      if (!accepted) return { status: "skipped" };
      // Publish without awaiting the consumer: claiming must not hold a scan slot.
      // A quiet read (background re-read of a failed row) updates the row but must
      // never reach the claim queue: that path submits a real claim.
      const payload = {
        id: ticket.id,
        revision: ticket.revision,
        plans: response.plans || [],
        kind: ticket.kind,
        liveHash: response.liveHash || null,
      };
      if (ticket.quiet) onQuietResult(payload);
      else onResult(payload);
      return {
        status: "ok",
        plans: response.plans || [],
        activationError: response.activationError || null,
      };
    } catch (error) {
      const scope = parseScopeError(error);
      if (scope) return { status: "scope_changed", notStarted: scope.notStarted };
      setModel(settleClaimRequest(model, ticket, { error: errorText(error) }));
      return { status: "failed", error: errorText(error) };
    }
  }

  function start(id, kind, { retried = false, priority, quiet = false } = {}) {
    if (!live.some((a) => a.id === id)) return Promise.resolve({ status: "skipped" });
    const begun = beginClaimRequest(model, id, kind);
    if (!begun.ticket) return Promise.resolve({ status: "skipped" });
    setModel(begun.model);
    const ticket = begun.ticket;
    ticket.retried = retried;
    ticket.quiet = quiet;
    const key = claimKey(ticket);
    const rank = priority || (kind === "refresh" ? "manual" : "background");
    const started = queue.enqueue(key, () => run(ticket), { priority: rank });
    const promise = guard(ticket, key, started).then(async (outcome) => {
      const pending = inFlight.get(id);
      if (pending?.key === key) inFlight.delete(id);
      if (outcome.status === "scope_changed" || outcome.status === "skipped") {
        // Never leave the row busy when the request ended without a result.
        setModel(releaseClaimRequest(model, ticket));
      }
      if (outcome.status === "scope_changed" && outcome.notStarted && !retried && live.some((a) => a.id === id)) {
        await refreshState();
        // The retry must stay quiet: a quiet re-read that escalated into a normal one
        // would publish its result and submit a real claim.
        return start(id, kind, { retried: true, priority: rank, quiet: ticket.quiet });
      }
      emit();
      return outcome;
    });
    inFlight.set(id, { key, kind, ticket, promise });
    emit();
    return promise;
  }

  function preview(id, { priority, quiet = false } = {}) {
    const pending = inFlight.get(id);
    if (pending?.kind === "refresh") return pending.promise;
    if (pending?.kind === "preview") {
      queue.promote(pending.key);
      return pending.promise;
    }
    return start(id, "preview", { priority, quiet });
  }

  function refresh(id) {
    const pending = inFlight.get(id);
    if (pending?.kind === "refresh") return pending.promise;
    if (pending?.kind === "preview") {
      return pending.promise.catch(() => {}).then(() => refresh(id));
    }
    return start(id, "refresh");
  }

  async function refreshMany(ids, { onItem = () => {} } = {}) {
    let done = 0;
    const total = ids.length;
    return Promise.all(ids.map(async (id) => {
      const outcome = await refresh(id).catch((error) => ({ status: "failed", error: errorText(error) }));
      done += 1;
      onItem({ id, done, total, outcome });
      return outcome;
    }));
  }

  function invalidate(id) {
    cancel(id);
    setModel(invalidateClaimEntry(model, id));
  }

  async function afterClaim(id) {
    invalidate(id);
    return preview(id);
  }

  return {
    applyAccounts,
    preview,
    refresh,
    refreshMany,
    invalidate,
    afterClaim,
    entries: () => model.entries,
    busy: (id) => Boolean(model.entries[id]?.busy),
    inFlight: () => inFlight.size,
  };
}

export { CANCELLED };
