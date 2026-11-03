// Legacy claim data has no identity prefix: `a1|p1` (marks) and scope-less catalog
// entries cannot be attributed to any login. Blindly dropping them would let automatic
// claiming retry a plan the user already received; blindly attributing them to the
// current login would hide a new login's eligibility. So they are kept aside, block
// automatic claiming until the user decides, and can be confirmed or discarded — both
// decisions are explicit and testable.

export const isScopedKey = (key) => key.startsWith("live:") || key.startsWith("acct:");

/** Split persisted marks into identity-scoped keys and unattributable legacy records. */
export function splitMarkRecords(raw) {
  const scoped = {};
  const legacy = [];
  for (const [key, at] of Object.entries(raw || {})) {
    if (isScopedKey(key)) {
      scoped[key] = at;
      continue;
    }
    // 旧格式：`<accountId>|<planId>`
    const cut = key.lastIndexOf("|");
    if (cut <= 0) continue;
    legacy.push({
      key,
      accountId: key.slice(0, cut),
      planId: key.slice(cut + 1),
      at: Number(at) || 0,
    });
  }
  return { scoped, legacy };
}

export function legacyMarksFor(legacy, accountId) {
  return (legacy || []).filter((record) => record.accountId === accountId);
}

/** True when a legacy record covers this plan; such plans must not be auto-claimed. */
export function hasLegacyMark(legacy, accountId, planId) {
  return legacyMarksFor(legacy, accountId).some((record) => record.planId === planId);
}

/**
 * Explicit decision: the user confirms these records belong to the login shown now.
 * The mark is re-keyed to the identity scope (so it keeps suppressing) and the legacy
 * keys are removed.
 */
export function confirmLegacyMarks(marks, legacy, accountId, scopeKey) {
  const next = { ...(marks || {}) };
  const kept = [];
  const confirmed = [];
  for (const record of legacy || []) {
    if (record.accountId !== accountId) {
      kept.push(record);
      continue;
    }
    const key = `${scopeKey}|${record.planId}`;
    next[key] = record.at || Date.now();
    confirmed.push(key);
  }
  return { marks: next, legacy: kept, confirmed };
}

/** Explicit decision: the user says these records are not this login's; discard them. */
export function discardLegacyMarks(legacy, accountId) {
  const kept = [];
  const removed = [];
  for (const record of legacy || []) {
    if (record.accountId === accountId) removed.push(record.key);
    else kept.push(record);
  }
  return { legacy: kept, removed };
}

/** Catalog entries without a scope are legacy; they are never rendered as current. */
export function splitCatalogRecords(raw) {
  const scoped = {};
  const legacy = {};
  for (const [key, entry] of Object.entries(raw || {})) {
    if (entry?.scope) scoped[key] = entry;
    else legacy[key] = entry;
  }
  return { scoped, legacy };
}

export function legacyCatalogFor(legacy, accountId) {
  return Object.values(legacy || {}).filter((entry) => entry?.accountId === accountId);
}

/** Confirming catalog entries re-stamps them with the current identity scope. */
export function confirmLegacyCatalog(catalog, legacy, accountId, scopeKey) {
  const next = { ...(catalog || {}) };
  const kept = {};
  for (const [key, entry] of Object.entries(legacy || {})) {
    if (entry?.accountId !== accountId) {
      kept[key] = entry;
      continue;
    }
    const planId = entry.planId || key.split("|").pop();
    next[`${scopeKey}|${planId}`] = { ...entry, scope: scopeKey };
  }
  return { catalog: next, legacy: kept };
}

export function discardLegacyCatalog(legacy, accountId) {
  const kept = {};
  for (const [key, entry] of Object.entries(legacy || {})) {
    if (entry?.accountId !== accountId) kept[key] = entry;
  }
  return { legacy: kept };
}
