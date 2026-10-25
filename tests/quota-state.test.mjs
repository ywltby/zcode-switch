import test from "node:test";
import assert from "node:assert/strict";
import { createQuotaState, syncQuotaAccounts, beginQuotaRequest, settleQuotaRequest } from "../src/quota-state.js";

const accounts = (revision = "r1") => [{ id: "a", quota_revision: revision }, { id: "b", quota_revision: "b1" }];
const data = (revision = "r1") => ({ account_id: "a", revision, completion: "complete", refreshed_at: 100, plans: [{ name: "Synthetic plan" }] });
const initial = () => syncQuotaAccounts(createQuotaState(), accounts());

test("credential update rejects both late success and late error", () => {
  const first = beginQuotaRequest(initial(), "a");
  assert.ok(first.ticket);
  const updated = syncQuotaAccounts(first.model, accounts("r2"));
  assert.equal(updated.entries.a.data, undefined);
  for (const result of [{ data: data() }, { error: "old failure" }]) {
    assert.strictEqual(settleQuotaRequest(updated, first.ticket, result), updated);
  }
});

test("delete and re-add cannot accept an earlier response even with the same revision", () => {
  const first = beginQuotaRequest(initial(), "a");
  const removed = syncQuotaAccounts(first.model, accounts().slice(1));
  assert.equal(removed.entries.a, undefined);
  assert.strictEqual(settleQuotaRequest(removed, first.ticket, { data: data() }), removed);
  const readded = beginQuotaRequest(syncQuotaAccounts(removed, accounts()), "a");
  assert.notEqual(readded.ticket.sequence, first.ticket.sequence);
  assert.strictEqual(settleQuotaRequest(readded.model, first.ticket, { error: "late" }), readded.model);
});

test("manual and automatic refresh share a pending request", () => {
  const manual = beginQuotaRequest(initial(), "a");
  const automatic = beginQuotaRequest(manual.model, "a");
  assert.ok(manual.ticket);
  assert.equal(automatic.ticket, null);
  assert.strictEqual(automatic.model, manual.model);
});

test("same revision failure retains the previous value and original timestamp", () => {
  const first = beginQuotaRequest(initial(), "a");
  const loaded = settleQuotaRequest(first.model, first.ticket, { data: data() });
  const next = beginQuotaRequest(loaded, "a");
  const failed = settleQuotaRequest(next.model, next.ticket, { error: "network_error" });
  assert.deepEqual(failed.entries.a.data, data());
  assert.equal(failed.entries.a.err, "network_error");
  assert.equal(failed.entries.a.busy, false);
  assert.deepEqual(failed.entries.b, initial().entries.b);
});

test("mismatching server account or revision cannot overwrite a row", () => {
  const first = beginQuotaRequest(initial(), "a");
  for (const response of [{ ...data(), account_id: "b" }, data("old")]) {
    assert.strictEqual(settleQuotaRequest(first.model, first.ticket, { data: response }), first.model);
  }
});

test("rename preserves cached quota; a settled request cannot finish twice", () => {
  const first = beginQuotaRequest(initial(), "a");
  const loaded = settleQuotaRequest(first.model, first.ticket, { data: data() });
  const renamed = syncQuotaAccounts(loaded, accounts().map(a => ({ ...a, name: "Renamed" })));
  assert.deepEqual(renamed.entries.a.data, data());
  assert.strictEqual(settleQuotaRequest(renamed, first.ticket, { error: "duplicate" }), renamed);
});
