// Bounded request scheduler shared by quota and claim refreshes.
// It knows nothing about accounts, Tauri, or tokens; callers own request validity.

export const CANCELLED = Object.freeze({ status: "cancelled" });

const RANK = { background: 0, normal: 1, manual: 2 };
const rankOf = (priority) => RANK[priority] ?? RANK.normal;

export function createRequestQueue(limit = 3) {
  const max = Math.max(1, Math.floor(Number(limit) || 1));
  const queued = new Map();
  const running = new Map();
  let active = 0;
  let order = 0;
  let promoted = 0;

  function drain() {
    while (active < max && queued.size > 0) {
      let next = null;
      for (const job of queued.values()) {
        if (!next || job.rank > next.rank || (job.rank === next.rank && job.order < next.order)) next = job;
      }
      if (!next) return;
      start(next);
    }
  }

  function start(job) {
    queued.delete(job.key);
    running.set(job.key, job);
    active++;
    let result;
    try {
      result = job.task();
    } catch (error) {
      result = Promise.reject(error);
    }
    Promise.resolve(result).then(
      (value) => { finish(job); job.resolve(value); },
      (error) => { finish(job); job.reject(error); },
    );
  }

  function finish(job) {
    if (!job.open) return;
    job.open = false;
    running.delete(job.key);
    active--;
    drain();
  }

  function enqueue(key, task, { priority = "normal" } = {}) {
    const existing = queued.get(key) || running.get(key);
    if (existing) {
      if (queued.has(key)) {
        existing.rank = Math.max(existing.rank, rankOf(priority));
        if (priority === "manual") promote(key);
      }
      return existing.promise;
    }
    let resolve, reject;
    const promise = new Promise((res, rej) => { resolve = res; reject = rej; });
    const job = { key, task, priority, rank: rankOf(priority), order: ++order, open: true, promise, resolve, reject };
    queued.set(key, job);
    drain();
    return promise;
  }

  function promote(key) {
    const job = queued.get(key);
    if (!job) return false;
    job.rank = RANK.manual;
    job.priority = "manual";
    job.order = -++promoted;
    return true;
  }

  function cancelQueued(key) {
    const job = queued.get(key);
    if (!job) return false;
    queued.delete(key);
    job.open = false;
    job.resolve(CANCELLED);
    return true;
  }

  return {
    enqueue,
    promote,
    cancelQueued,
    activeCount: () => active,
    queuedCount: () => queued.size,
    limit: max,
  };
}
