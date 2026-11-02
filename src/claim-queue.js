// Successful eligibility reads feed this queue without awaiting its consumer.
// Scan slots belong to claim-flow; this queue owns only one claim at a time.
export function createClaimQueue({
  run,
  canStart = () => true,
  allowAccount = () => true,
  onChange = () => {},
  onSettled = () => {},
  now = Date.now,
  gapMs = 5000,
  retryDelayMs = 10 * 60 * 1000,
  perAccountLimit = 5,
}) {
  let accounts = new Map(), generation = 0;
  let queued = [], active = null, timer = null, nextAt = 0;
  const eligibility = new Map(), recent = new Map(), cooldowns = new Map();
  const current = job => accounts.get(job.id)?.epoch === job.epoch;
  const keyOf = (owner, planId) => JSON.stringify([owner.id, owner.epoch, planId]);
  const eligible = job => current(job) && eligibility.get(job.id)?.has(job.plan.plan_id);
  const emit = () => onChange();

  function prune() {
    for (const [key, entry] of recent) if (!current(entry) || entry.until <= now()) recent.delete(key);
    for (const [id, entry] of cooldowns) if (!current(entry) || entry.until <= now()) cooldowns.delete(id);
    queued = queued.filter(eligible);
  }

  function allowed(job) {
    return allowAccount(job.id) && !cooldowns.has(job.id)
      && !recent.has(job.key)
      && [...recent.values()].filter(entry => entry.epoch === job.epoch).length < perAccountLimit;
  }

  function kick() {
    prune();
    if (active || !queued.length || !canStart()) return;
    if (now() < nextAt) {
      if (!timer) timer = setTimeout(() => { timer = null; kick(); }, nextAt - now());
      return;
    }
    let job;
    while (queued.length && !job) {
      const next = queued.shift();
      if (allowed(next)) job = next;
    }
    if (!job) { emit(); return; }
    active = job;
    recent.set(job.key, { id: job.id, epoch: job.epoch, until: now() + retryDelayMs });
    emit();
    // Reserve the worker synchronously; execute in a microtask so publishing a
    // result cannot hold the query's slot even if the consumer throws.
    Promise.resolve().then(() => eligible(job) && allowAccount(job.id)
      ? run({ ...job, isCurrent: () => current(job) }) : { status: "skipped" }).catch(error => ({ status: "failed", error }))
      .then(outcome => {
        outcome ||= { status: "failed" };
        if (current(job)) {
          if (outcome.status !== "ok" && outcome.status !== "skipped") {
            cooldowns.set(job.id, { id: job.id, epoch: job.epoch,
              until: Math.max(cooldowns.get(job.id)?.until || 0, outcome.cooldownUntil || now() + retryDelayMs) });
          }
          if (outcome.status === "skipped") recent.delete(job.key);
          const seen = recent.get(job.key);
          if (seen) seen.until = Math.max(seen.until, now() + retryDelayMs);
        }
        onSettled(job, outcome);
      }).finally(() => {
        active = null;
        nextAt = now() + gapMs;
        emit();
        kick();
      });
  }

  function applyAccounts(list) {
    const next = new Map();
    for (const account of list || []) {
      const old = accounts.get(account.id);
      next.set(account.id, old?.revision === account.quota_revision ? old
        : { id: account.id, revision: account.quota_revision, epoch: ++generation });
    }
    for (const [id, owner] of accounts) if (next.get(id)?.epoch !== owner.epoch) eligibility.delete(id);
    accounts = next;
    prune();
    kick();
  }

  function offer({ id, revision, plans = [] }) {
    const owner = accounts.get(id);
    if (!owner || owner.revision !== revision) return;
    const valid = plans.filter(plan => typeof plan?.plan_id === "string" && plan.plan_id);
    eligibility.set(id, new Set(valid.map(plan => plan.plan_id)));
    prune();
    for (const plan of valid.slice(0, perAccountLimit)) {
      const key = keyOf(owner, plan.plan_id);
      const job = { ...owner, key, plan: { ...plan } };
      if (!allowed(job) || active?.key === key || queued.some(item => item.key === key)) continue;
      queued.push(job);
    }
    emit();
    kick();
  }

  function record({ id, revision, planId }, outcome) {
    const owner = accounts.get(id);
    if (!owner || owner.revision !== revision) return;
    const key = keyOf(owner, planId);
    recent.set(key, { ...owner, until: now() + retryDelayMs });
    if (outcome.status !== "ok" && outcome.status !== "skipped") {
      cooldowns.set(id, { ...owner,
        until: Math.max(cooldowns.get(id)?.until || 0, outcome.cooldownUntil || now() + retryDelayMs) });
    }
    queued = queued.filter(job => job.key !== key);
    emit();
  }

  return {
    applyAccounts, offer, kick, record,
    pendingCount: () => queued.length,
    activeId: () => active?.id || null,
    status: id => active?.id === id && current(active) ? "running"
      : queued.some(job => job.id === id) ? "queued" : null,
  };
}
