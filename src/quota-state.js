// Request ownership is independent of rendering and Tauri.
export const createQuotaState = () => ({ entries: {}, sequence: 0 });
export function syncQuotaAccounts(model, accounts) {
  const entries = {};
  for (const account of accounts) {
    const old = model.entries[account.id];
    entries[account.id] = old?.revision === account.quota_revision
      ? old : { revision: account.quota_revision, busy: false };
  }
  return { ...model, entries };
}

export function beginQuotaRequest(model, id) {
  const entry = model.entries[id];
  if (!entry?.revision || entry.busy) return { model, ticket: null };
  const sequence = model.sequence + 1;
  const ticket = { id, revision: entry.revision, sequence };
  return {
    ticket,
    model: { sequence, entries: { ...model.entries, [id]: { ...entry, sequence, busy: true } } },
  };
}

export function settleQuotaRequest(model, ticket, { data, error } = {}) {
  const entry = ticket && model.entries[ticket.id];
  if (!entry?.busy || entry.revision !== ticket.revision || entry.sequence !== ticket.sequence) return model;
  if (data && (data.account_id !== ticket.id || data.revision !== ticket.revision)) return model;
  return {
    ...model,
    entries: { ...model.entries, [ticket.id]: {
      ...entry, busy: false, data: error ? entry.data : data, err: error || null,
    } },
  };
}
