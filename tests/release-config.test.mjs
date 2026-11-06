import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const yml = readFileSync(new URL("../.github/workflows/release.yml", import.meta.url), "utf8");

// The workflow keeps the pre-merge release identity: a hyphen in the tag means a
// pre-release (beta/rc), a clean tag is a normal release. The wjs fork's fixed
// "v1.7.3-Z.EMAIL" notes and "makeLatest" switch must not leak back in.
const prereleaseRule = yml.match(/^\s*prerelease:\s*(.+)$/m)?.[1]?.trim() ?? "";
const isPrerelease = (tag) => tag.includes("-");

test("release workflow uses the pre-merge prerelease rule", () => {
  assert.equal(prereleaseRule, "${{ contains(inputs.tag || github.ref_name, '-') }}");
});

test("beta, rc and final tags map to the expected release state", () => {
  assert.equal(isPrerelease("v1.7.0-beta.13"), true);
  assert.equal(isPrerelease("v1.7.0-rc.1"), true);
  assert.equal(isPrerelease("v1.7.0"), false);
  assert.equal(isPrerelease("v1.7.0-Z.EMAIL"), true, "带后缀的标签仍按预发布处理");
});

test("foreign release identity is gone and the project notes are back", () => {
  assert.ok(!yml.includes("makeLatest"), "makeLatest 是 wjs 的 Latest 覆盖设置");
  assert.ok(!yml.includes("v1.7.3-Z.EMAIL"), "不得保留 wjs 的固定版本说明");
  assert.ok(!yml.includes("pjpv/zcode-switch"), "不得保留上游 fork 的固定介绍");
  assert.ok(yml.includes("blob/main/README.md"), "恢复本仓库的 README 入口");
  assert.ok(yml.includes("checksums-"), "保留校验和说明");
  assert.ok(yml.includes("## 未签名提示"), "保留未签名提示段落");
});

test("release body pulls the per-version notes from docs/releases", () => {
  assert.ok(yml.includes('docs/releases/${RELEASE_TAG}.md'), "按标签读取本版说明");
  assert.ok(yml.includes("${{ steps.notes.outputs.body }}"), "说明注入到 releaseBody");
});
