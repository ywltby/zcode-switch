// Offline harness: real quota scheduler and quota state, mocked IPC boundary.
// The scheduler under test is never re-implemented here.

import { createQuotaScheduler } from "../src/refresh-scheduler.js";

export const PERIOD = 5 * 60 * 1000;

export function account(id, revision = "r1", name = `Synthetic ${id}`) {
  return { id, name, quota_revision: revision, is_active: false, has_config: true, has_user_info: true };
}

export function quotaPayload(id, revision, overrides = {}) {
  return {
    account_id: id,
    revision,
    completion: "complete",
    errors: [],
    refreshed_at: 1_700_000_000_000,
    is_empty: false,
    items: [],
    plans: [{ plan_id: "plan-1", name: "Synthetic plan", tier: "Pro", tier_code: "pro" }],
    ...overrides,
  };
}

const tick = () => new Promise(setImmediate);

export function createHarness({ accounts = [], limit = 3, now } = {}) {
  const clock = { value: 0 };
  const control = {
    accounts: accounts.map((a) => ({ ...a })),
    hold: new Set(),
    failures: new Map(),
    pending: new Map(),
    calls: [],
    active: 0,
    peak: 0,
    settled: [],
  };

  const invoke = (command, args = {}) => {
    control.calls.push({ command, args, at: clock.value });
    if (command !== "get_account_quota") return Promise.reject(new Error(`unexpected command: ${command}`));
    const id = args.id;
    control.active++;
    control.peak = Math.max(control.peak, control.active);
    const done = () => { control.active--; };
    if (control.hold.has(id) || control.failures.has(id)) {
      return new Promise((resolve, reject) => {
        control.pending.set(id, {
          id,
          revision: args.revision,
          settle(error) {
            control.pending.delete(id);
            done();
            if (error) reject(error); else resolve(quotaPayload(id, args.revision));
          },
        });
      });
    }
    return Promise.resolve(quotaPayload(id, args.revision)).finally(done);
  };

  const scheduler = createQuotaScheduler({
    invoke,
    now: now || (() => clock.value),
    limit,
    periodMs: PERIOD,
    random: () => 0.5,
    classifyData: (data) => (data.completion === "unavailable" ? "partial" : null),
    errorText: (error) => String(error),
    onChange: () => {},
  });
  scheduler.applyAccounts(control.accounts);

  const harness = {
    scheduler,
    control,
    accounts: () => control.accounts,
    callsTo: (id) => control.calls.filter((c) => c.command === "get_account_quota" && c.args.id === id),
    holdAll() { for (const a of control.accounts) control.hold.add(a.id); },
    release(id) { const p = control.pending.get(id); if (!p) throw new Error(`nothing pending for ${id}`); p.settle(); },
    fail(id, error) { const p = control.pending.get(id); if (!p) throw new Error(`nothing pending for ${id}`); p.settle(error); },
    async releaseNext() {
      const first = control.pending.keys().next();
      if (first.done) return false;
      harness.release(first.value);
      await tick();
      return true;
    },
    async drain(limitRounds = 200) {
      let rounds = 0;
      while (control.pending.size && rounds++ < limitRounds) await harness.releaseNext();
      await tick();
      return rounds;
    },
    advance(ms) { clock.value += ms; },
    rename(id, name) {
      control.accounts = control.accounts.map((a) => (a.id === id ? { ...a, name } : a));
      scheduler.applyAccounts(control.accounts);
    },
    bumpRevision(id, revision = "r2") {
      control.accounts = control.accounts.map((a) => (a.id === id ? { ...a, quota_revision: revision } : a));
      scheduler.applyAccounts(control.accounts);
    },
    remove(id) {
      control.accounts = control.accounts.filter((a) => a.id !== id);
      scheduler.applyAccounts(control.accounts);
    },
    readd(id, revision = "r1") {
      control.accounts = [...control.accounts, account(id, revision)];
      scheduler.applyAccounts(control.accounts);
    },
    state(id) { return scheduler.entries()[id]; },
    peak: () => control.peak,
    active: () => control.active,
  };
  return harness;
}

export { tick };
