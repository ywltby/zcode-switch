// Low-frequency re-read for rows that currently show an error.
//
// Eligibility used to refresh only while the auto-claim switch was on, so a failed
// preview stayed on screen for as long as that switch stayed off. Re-reading every row
// is not the answer either: repeated probes keep an upstream rate limit alive. This
// module only selects rows that already failed, and backs off on repeated failures.

export const STALE_PREVIEW_INTERVAL_MS = 30 * 60 * 1000;
export const STALE_PREVIEW_CAP_MS = 2 * 60 * 60 * 1000;

export function createStalePreview({
  intervalMs = STALE_PREVIEW_INTERVAL_MS,
  capMs = STALE_PREVIEW_CAP_MS,
  max = 3,
} = {}) {
  // id -> { fails, nextAt }
  const state = new Map();
  const limit = Math.max(1, Math.floor(Number(max) || 1));
  const due = (id, now) => (state.get(id)?.nextAt ?? 0) <= now;

  // An error that just appeared gets a full interval before its first re-read. Without
  // this a brand-new failure is "due" immediately and gets probed on the very next
  // tick, which is exactly the probing that keeps an upstream rate limit alive.
  function prime(entries, { now = 0 } = {}) {
    if (!entries) return;
    for (const id of Object.keys(entries)) {
      if (!entries[id]?.err || state.has(id)) continue;
      state.set(id, { fails: 0, nextAt: now + intervalMs });
    }
  }

  // A row that stopped showing an error has recovered, possibly through a manual or
  // routine read this module never saw. Drop its schedule so the next failure waits a
  // full interval instead of inheriting the previous error's retry time.
  function sync(entries) {
    if (!entries) return;
    for (const id of [...state.keys()]) {
      if (!entries[id]?.err) state.delete(id);
    }
  }

  function targets(entries, { now = 0, eligible = () => true } = {}) {
    if (!entries) return [];
    return Object.keys(entries)
      .filter((id) => entries[id]?.err)
      .filter((id) => eligible(id) && due(id, now))
      .sort((a, b) => (state.get(a)?.nextAt ?? 0) - (state.get(b)?.nextAt ?? 0) || (a < b ? -1 : 1))
      .slice(0, limit);
  }

  // Success clears the backoff; repeated failures wait 30m, 1h, then 2h (capped).
  function settle(id, ok, now) {
    if (ok) {
      state.delete(id);
      return;
    }
    const fails = (state.get(id)?.fails ?? 0) + 1;
    const delay = Math.min(intervalMs * 2 ** Math.min(fails - 1, 3), capMs);
    state.set(id, { fails, nextAt: now + delay });
  }

  return { prime, sync, targets, settle };
}
