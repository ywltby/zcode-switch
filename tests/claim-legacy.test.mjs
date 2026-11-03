import test from "node:test";
import assert from "node:assert/strict";

import {
  confirmLegacyCatalog,
  confirmLegacyMarks,
  discardLegacyCatalog,
  discardLegacyMarks,
  hasLegacyMark,
  isScopedKey,
  legacyCatalogFor,
  legacyMarksFor,
  splitCatalogRecords,
  splitMarkRecords,
} from "../src/claim-legacy.js";

test("legacy marks are separated and still block their own account", () => {
  const { scoped, legacy } = splitMarkRecords({
    "acct:scope-a|p1": 10,
    "live:scope-l|p2": 20,
    "a1|p3": 30,
    "a2|p4": 40,
    "broken": 50,
  });
  assert.deepEqual(Object.keys(scoped).sort(), ["acct:scope-a|p1", "live:scope-l|p2"]);
  assert.deepEqual(
    legacy.map((r) => `${r.accountId}/${r.planId}`).sort(),
    ["a1/p3", "a2/p4"],
    "旧键按 accountId|planId 解析，无法解析的条目被丢弃",
  );
  assert.equal(hasLegacyMark(legacy, "a1", "p3"), true);
  assert.equal(hasLegacyMark(legacy, "a1", "p4"), false, "不同账号不得互相压制");
  assert.equal(isScopedKey("acct:scope-a|p1"), true);
  assert.equal(isScopedKey("a1|p3"), false);
});

test("confirming legacy marks re-keys them to the current identity scope", () => {
  const { legacy } = splitMarkRecords({ "a1|p1": 10, "a2|p2": 20 });
  const result = confirmLegacyMarks({}, legacy, "a1", "acct:scope-b");
  assert.deepEqual(result.confirmed, ["acct:scope-b|p1"]);
  assert.equal(result.marks["acct:scope-b|p1"], 10, "保留原始时间戳");
  assert.deepEqual(result.legacy.map((r) => r.key), ["a2|p2"], "其它账号的旧记录不动");
  assert.equal(hasLegacyMark(result.legacy, "a1", "p1"), false);
});

test("discarding legacy marks removes only that account's records", () => {
  const { legacy } = splitMarkRecords({ "a1|p1": 10, "a2|p2": 20 });
  const result = discardLegacyMarks(legacy, "a1");
  assert.deepEqual(result.removed, ["a1|p1"]);
  assert.deepEqual(result.legacy.map((r) => r.key), ["a2|p2"]);
  assert.equal(legacyMarksFor(result.legacy, "a1").length, 0);
});

test("scope-less catalog entries are legacy and never rendered as current", () => {
  const { scoped, legacy } = splitCatalogRecords({
    "acct:scope-a|p1": { accountId: "a1", scope: "acct:scope-a", planId: "p1", title: "New" },
    "a1|p2": { accountId: "a1", planId: "p2", title: "Old" },
  });
  assert.deepEqual(Object.keys(scoped), ["acct:scope-a|p1"]);
  assert.equal(legacyCatalogFor(legacy, "a1").length, 1);
  const confirmed = confirmLegacyCatalog({}, legacy, "a1", "acct:scope-b");
  assert.equal(confirmed.catalog["acct:scope-b|p2"].scope, "acct:scope-b");
  assert.equal(confirmed.catalog["acct:scope-b|p2"].title, "Old");
  const discarded = discardLegacyCatalog(legacy, "a1");
  assert.equal(Object.keys(discarded.legacy).length, 0);
});
