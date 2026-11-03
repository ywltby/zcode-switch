// Claim eligibility ownership: which preview/refresh result may land in a row.
// No rendering, no IPC, no general task framework.

const entryFor = (account) => ({ revision: account.quota_revision, busy: false, plans: [], err: null, activationError: null });

export const createClaimState = () => ({ entries: {}, sequence: 0 });

export function syncClaimAccounts(model, accounts) {
  const entries = {};
  for (const account of accounts) {
    const old = model.entries[account.id];
    entries[account.id] = old?.revision === account.quota_revision ? old : entryFor(account);
  }
  return { ...model, entries };
}

export function beginClaimRequest(model, id, kind) {
  const entry = model.entries[id];
  if (!entry?.revision || entry.busy) return { model, ticket: null };
  const sequence = model.sequence + 1;
  const ticket = { id, revision: entry.revision, sequence, kind };
  return {
    ticket,
    model: {
      sequence,
      entries: { ...model.entries, [id]: { ...entry, sequence, busy: true, kind } },
    },
  };
}

export function releaseClaimRequest(model, ticket) {
  const entry = ticket && model.entries[ticket.id];
  if (!entry?.busy || entry.revision !== ticket.revision || entry.sequence !== ticket.sequence) return model;
  return { ...model, entries: { ...model.entries, [ticket.id]: { ...entry, busy: false } } };
}

export function settleClaimRequest(model, ticket, { plans, error, activationError } = {}) {
  const entry = ticket && model.entries[ticket.id];
  if (!entry?.busy || entry.revision !== ticket.revision || entry.sequence !== ticket.sequence) return model;
  const next = { ...entry, busy: false, kind: null };
  if (error) {
    next.err = error;
    next.activationError = activationError || null;
  } else {
    next.plans = plans || [];
    next.err = null;
    next.activationError = activationError || null;
  }
  return { ...model, entries: { ...model.entries, [ticket.id]: next } };
}

export function invalidateClaimEntry(model, id) {
  const entry = model.entries[id];
  if (!entry) return model;
  return { ...model, entries: { ...model.entries, [id]: { ...entryFor({ quota_revision: entry.revision }) } } };
}

/**
 * Adopt the revision the backend reported for a row. The live login has no stored
 * revision, so its first eligibility read is what binds later claims to a concrete
 * backend account generation. Ignored while a request is in flight: accepting it then
 * would invalidate the very ticket that produced it.
 */
export function setClaimRevision(model, id, revision) {
  const entry = model.entries[id];
  if (!entry || entry.busy || !revision || entry.revision === revision) return model;
  return { ...model, entries: { ...model.entries, [id]: { ...entry, revision } } };
}
