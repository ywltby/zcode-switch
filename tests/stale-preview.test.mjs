import test from "node:test";
import assert from "node:assert/strict";
import { createStalePreview, STALE_PREVIEW_CAP_MS, STALE_PREVIEW_INTERVAL_MS } from "../src/stale-preview.js";

const MIN = 60 * 1000;
const rows = (map) => Object.fromEntries(
  Object.entries(map).map(([id, err]) => [id, { err, busy: false, plans: [] }]),
);

test("only rows that show an error are re-read", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "preview 请求失败 HTTP 429: ", b: null, c: "HTTP 500" });
  assert.deepEqual(stale.targets(entries, { now: 0 }), ["a", "c"]);
  assert.deepEqual(stale.targets(rows({ a: null }), { now: 0 }), []);
  assert.deepEqual(stale.targets(null, { now: 0 }), []);
});

test("an unsuccessful re-read waits a full interval before the next one", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "HTTP 429" });
  stale.settle("a", false, 0);
  assert.deepEqual(stale.targets(entries, { now: STALE_PREVIEW_INTERVAL_MS - MIN }), []);
  assert.deepEqual(stale.targets(entries, { now: STALE_PREVIEW_INTERVAL_MS }), ["a"]);
});

test("repeated failures back off 30m, 1h, then the cap", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "HTTP 429" });
  stale.settle("a", false, 0);
  assert.deepEqual(stale.targets(entries, { now: 30 * MIN }), ["a"], "first retry after 30m");

  stale.settle("a", false, 30 * MIN);
  assert.deepEqual(stale.targets(entries, { now: 89 * MIN }), [], "second retry waits 60m");
  assert.deepEqual(stale.targets(entries, { now: 90 * MIN }), ["a"]);

  stale.settle("a", false, 90 * MIN);
  assert.deepEqual(stale.targets(entries, { now: 90 * MIN + STALE_PREVIEW_CAP_MS - MIN }), []);
  assert.deepEqual(stale.targets(entries, { now: 90 * MIN + STALE_PREVIEW_CAP_MS }), ["a"]);

  // The cap holds no matter how often it keeps failing.
  stale.settle("a", false, 0);
  stale.settle("a", false, 0);
  stale.settle("a", false, 0);
  stale.settle("a", false, 0);
  assert.deepEqual(stale.targets(entries, { now: STALE_PREVIEW_CAP_MS }), ["a"]);
});

test("a successful re-read clears the backoff immediately", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "HTTP 429" });
  stale.settle("a", false, 0);
  assert.deepEqual(stale.targets(entries, { now: MIN }), []);
  stale.settle("a", true, MIN);
  assert.deepEqual(stale.targets(entries, { now: MIN }), ["a"]);
});

test("ineligible rows and rows already in flight are skipped", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "HTTP 429", b: "HTTP 429" });
  assert.deepEqual(stale.targets(entries, { now: 0, eligible: (id) => id !== "a" }), ["b"]);
  assert.deepEqual(stale.targets(entries, { now: 0, eligible: () => false }), []);
});

test("a newly failed row waits a full interval before its first re-read", () => {
  const stale = createStalePreview();
  const entries = rows({ a: "HTTP 429" });
  stale.prime(entries, { now: 1000 });
  assert.deepEqual(stale.targets(entries, { now: 1000 }), [], "prime keeps a fresh error off this tick");
  assert.deepEqual(stale.targets(entries, { now: 1000 + STALE_PREVIEW_INTERVAL_MS - 1 }), []);
  assert.deepEqual(stale.targets(entries, { now: 1000 + STALE_PREVIEW_INTERVAL_MS }), ["a"]);

  // 重复 prime 不会把已排定的重试时间往后推。
  stale.prime(entries, { now: 1000 + STALE_PREVIEW_INTERVAL_MS });
  assert.deepEqual(stale.targets(entries, { now: 1000 + STALE_PREVIEW_INTERVAL_MS }), ["a"]);
});

test("a recovered row starts a fresh interval on its next error", () => {
  const stale = createStalePreview();
  const failing = rows({ a: "HTTP 429" });
  stale.prime(failing, { now: 0 });
  assert.deepEqual(stale.targets(failing, { now: STALE_PREVIEW_INTERVAL_MS }), ["a"], "the first error is scheduled");

  // 首次间隔内手动查询成功：这一行不再报错。
  stale.sync(rows({ a: null }));
  // 同一个间隔窗口内又出现新的 429：必须重新从头等，而不是沿用旧时间。
  stale.prime(failing, { now: 5 * MIN });
  assert.deepEqual(stale.targets(failing, { now: 5 * MIN }), [], "the new error must not reuse the old retry time");
  assert.deepEqual(stale.targets(failing, { now: 5 * MIN + STALE_PREVIEW_INTERVAL_MS }), ["a"]);
});

test("one round never asks for more rows than the limit", () => {
  const stale = createStalePreview({ max: 2 });
  const entries = rows({ a: "x", b: "x", c: "x", d: "x" });
  assert.deepEqual(stale.targets(entries, { now: 0 }), ["a", "b"]);
});
