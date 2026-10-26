# Concurrent Refresh and Claim Recovery Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 在保留账号额度隔离的前提下，引入最多三路并发的额度与领取资格刷新，并完善串行领取的验证码恢复、超时处理和迟到结果回填。

**Architecture:** 前端使用一个共享的有界请求队列，继续由账号版本和请求序号决定结果是否有效。实际领取保持全局串行，由后端管理带 `claimId` 的会话；界面等待结束与领取请求结束分开处理。验证码配置可缓存、有限重试，领取 POST 不自动重放。

**Tech Stack:** 原生 JavaScript / Vite / Node 内置测试运行器；Rust / Tauri 2 / ureq；Python Playwright 离线 UI 验收。

---

## 1. 状态与依据

- 状态：设计及实施计划，尚未开始实现。本文中的新增文件、接口和测试均为目标契约。
- 日期：2026-10-26。
- 工作基线：本仓库 `d27553fb2ed16c8734fc8a4994223550f5a60754`，`1.7.0-beta.4`。
- 参考：[pjpv/zcode-switch#28](https://github.com/pjpv/zcode-switch/pull/28)，审查 head 为 `c6419e06aa1310e5ea021ffec5d0d419ef0d5c12`。实施时以该固定版本为参考，后续 PR 变更另行核对。
- 该 PR 实现三路并发刷新，`claimAll` 和 `autoClaimTick` 的领取仍为串行。已独立运行其 14 项 Node 模拟测试，全部通过；这不是当前实现或真实线上领取的验收结果。
- 已复现该 PR 的问题：前端 waiter 超时后，迟到的 `claim://result` 成功事件被直接忽略，没有通知或后续刷新。后端取消窗口不能撤回已经发出的 POST。
- 延续[额度隔离计划](2026-10-25-quota-account-isolation.md)及[渠道标识设计](2026-10-26-account-channel-labels.md)。保留 beta.3 对已有项目 Key 的兼容修复。
- 本次交付为计划文档；不因编写计划执行领取、激活上报、发布或安装。

### 方案比较

| 方案 | 收益与代价 | 决策 |
| --- | --- | --- |
| 在 beta.4 上选择性接入队列与领取会话恢复 | 缩短刷新等待，保留现有隔离，需要补足竞态测试 | 采用 |
| 整包合并或覆盖 PR #28 的实现 | 接入表面较快，但旧额度路径及未处理的迟到结果与当前目标不符 | 不采用 |
| 多账号同时打开验证并提交领取 | 可能缩短领取耗时，但需要多会话、多窗口和服务端并发规则验证 | 本期不做 |

## 2. 产品行为与边界

1. `get_account_quota`、`claim_preview`、`claim_refresh` 共享 **3 个逻辑任务槽位**。这是应用层任务上限，不宣称所有底层 HTTP 请求的全局上限，也不覆盖 OAuth 等其他功能。
2. 自动额度扫描优先级低；手动刷新额度、刷新领取资格、单账号重试优先级高。同级 FIFO；手动请求可以提升尚未开始的重复任务，不能抢占正在执行的任务。
3. 同一账号、同一版本、同一轮请求只执行一次。不同请求类型不混用响应：普通预览不能代替包含激活上报的资格刷新。
4. 批量刷新完成一个账号才增加进度。失败不阻塞其他账号；同版本失败保留旧结果，并标明失败。删除或换版本的账号计为跳过，不能算成功。
5. 移除批量资格刷新账号间的 5 秒固定等待。11 个账号因此减少 50 秒固定等待，网络耗时另计；不承诺实际领取提速三倍。
6. 保留整批资格刷新 60 秒冷却。单账号失败重试不被整批冷却拦住，也不改变整批冷却截止时间；仍受请求去重、服务端限流与领取互斥约束。
7. 一键领取、自动领取和手动领取共用一个后端活动会话。保留一键领取的 1.2 秒间隔、自动领取的 5 秒账号间隔及现有业务冷却。
8. 本期自动领取循环仍逐账号“刷新资格→验证→领取”，不增加提前刷新所有账号的流水线。它通过共享队列调用刷新，但不把领取改成 `Promise.all`。
9. `claim_refresh` 会 POST 上报激活事件，既非纯 GET，也非无副作用查询；不得把它纳入普通额度扫描。失败后不整段自动重放激活上报。
10. 保留 `z.ai · 国际`、`bigmodel.cn · 国内` 徽标及中英文界面。不开设并发设置页，不调整自动领取默认开关。
11. 领取等待超时不等于领取失败。提交尚在执行时显示“已提交，正在确认结果”；传输中断且结果不明时显示“领取结果待确认”，不直接提供普通自动重试。
12. 不导入 PR 的 `candidate_tokens`、旧 config key 回退或旧 `quota_for_live` 实现，不放宽当前 `claim_token` 的严格 JWT 来源。

## 3. 并发刷新设计

### 3.1 队列与请求所有权

新增 `src/request-queue.js`，采用以下最小接口：

```js
const queue = createRequestQueue(3);
const promise = queue.enqueue(key, task, { priority: "manual" });
queue.promote(key);
queue.cancelQueued(key); // 仅移除未执行任务，返回可识别的 cancelled 结果
```

- 每个任务只在一个位置释放槽位，成功、异常、同步抛错和过期跳过均走同一收尾路径。
- 活跃请求不做虚假取消。若账号变更，允许网络结束，但丢弃其旧结果。
- 队列模块不读取账号、不调用 Tauri，也不持有 Token。
- key 用 `JSON.stringify([kind, id, revision, sequence])` 构造，避免字符串拼接歧义。
- 继续使用 `src/quota-state.js` 的 `beginQuotaRequest` / `settleQuotaRequest`。入队时获得 ticket，出队发 IPC 前再次确认 ticket 仍属于当前账号。
- 当前 entry 的 `sequence` 指向 promise 表。同一请求重复点击返回该 promise 并提升优先级；旧 promise 收尾只能删除自己的 key。
- 删除后重新添加即使 ID、revision 相同，也必须取得新的 sequence；不能命中旧 promise 或接受旧响应。账号改名不创建新代次。
- 不把后端 `account_id` / `revision` 检查简化为“只要 ID 相同就写入”。

### 3.2 领取资格的状态

新增 `src/claim-state.js`，只管理资格请求 ticket、缓存和结果接受规则；不引入通用任务框架。

- 对齐额度的 `{ id, revision, sequence }` 校验与账号删除处理，revision 使用后端已经提供的 `quota_revision`。
- 同一版本的 `claim_preview` 在途时，`claim_refresh` 等它结束后再入队；等待期间不占队列槽位。预览失败不阻止显式资格刷新。
- `claim_refresh` 在途时，普通预览复用其 plans 结果；两种方法对外返回形状仍分别保持清晰。
- 一次领取完成后，要使提交前启动的资格请求失效，再请求新预览，不能复用提交前的旧 promise。
- `applyState` 同步两套状态；更新、删除时同时取消未执行的旧任务。新增账号进入下一轮扫描，不动态扩大已经开始的一轮进度分母。
- 批量刷新使用固定 ticket 列表和逐项 `finally` 统计；结果分为成功、失败、跳过。账号切换只改变当前账号标识，不改变其他账号请求的所有权。

### 3.3 后端上下文与线程

- 将 `claim_preview`、`claim_refresh`、`claim_start` 的网络准备、验证码配置请求和领取提交移到 `tauri::async_runtime::spawn_blocking`。窗口操作与网络等待分开。
- `claim_preview` / `claim_refresh` 接收 `revision`，返回包含 `accountId`、`revision` 的对象；更新所有前端和测试调用点。
- 在短存储锁内读取并校验账号快照，再释放锁进行网络调用；返回前再次验证版本。提交时只用会话持有的不可变材料，不重新读取 live 凭证。
- 明确处理旧快照缺少 `virtual_device_mid` 的情况：领取路径沿用首次初始化设备 ID 的行为，但必须先校验传入版本，再在短锁内初始化；如果由此改变 revision，在任何激活上报、预览或领取 HTTP 之前返回结构化 `scope_changed`、`notStarted: true`。
- 前端收到这一特定结果后先刷新 state，允许用新 ticket 重新准备一次。普通鉴权错误、网络失败、已经发过激活上报的失败均不得走此自动重试分支。普通额度查询仍不能生成设备 ID。
- 不持有 `STORE_LOCK`、额度 I/O 锁或领取状态锁等待网络。短锁内调用应使用明确的 locked helper，避免再次调用会自行加锁的公开函数造成死锁。
- 429 不自动重放；复用现有适用的 Retry-After 解析策略，为对应账号/操作设置下一次可请求时间。单账号重试不能绕过它。

## 4. 串行领取及迟到结果

### 4.1 后端会话为准

新增 `src-tauri/src/claim_session.rs`，将 `lib.rs` 中的 `PendingClaim` 和全局互斥规则移入可独立测试的状态管理模块。

```rust
// 目标状态；数据字段按下文契约补齐。
enum ClaimPhase {
    Preparing,
    Verifying,
    Submitting,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
}
```

活动会话只允许一个，持有 `claimId`、账号 ID、revision、planId、阶段，以及不对外序列化的凭证快照与设备 ID。成功、失败、取消和结果不明记录可以保留，但不占用活动网络槽。

| 阶段 | 取消或窗口关闭 | 新领取是否允许 |
| --- | --- | --- |
| Preparing / Verifying | 标记取消；晚到的准备结果不得再打开窗口 | 清理该会话后允许 |
| Submitting | 只关闭展示或解除等待；不能撤回 POST、删除会话或提前释放领取互斥 | HTTP 任务结束前不允许 |
| Succeeded / Failed / Cancelled | 幂等，无重复通知或提交 | 按业务冷却允许 |
| Unknown | 保留待确认记录，先查询现状 | 其他账号可继续；该身份同套餐禁止自动再领 |

- `claim_start` 在网络准备前原子预留会话，避免两个调用同时通过空闲检查。相同 claimId 重入返回已有状态；另一个 claimId 返回 busy。
- `claim_captcha_submit` 原子地将 Verifying 改为 Submitting，只能成功一次。不能像 PR 一样 `take()` 后立即把活动会话清空。
- worker panic、IPC 失败、窗口创建失败都有明确收尾；提交前失败是 Failed，开始提交后的不可判定异常是 Unknown。
- HTTP 明确业务成功才标记 Succeeded；明确业务拒绝标记 Failed；已发出请求后的超时、响应丢失、无法解析响应等保守归为 Unknown。单纯预览变化不能证明某次 POST 成功或失败。
- `claimId` 仅用于本地会话关联，不假设服务器支持该 ID 的幂等语义。领取 POST 不设置自动重试，也要防止重定向导致隐式重放。

### 4.2 IPC、事件与窗口

目标接口：

```text
claim_start({id, revision, planId, claimId, auto}) -> session summary
claim_captcha_submit({claimId, param, region}) -> authoritative outcome
claim_cancel({claimId}) -> {phase, cancelled, submissionInFlight}
claim_status({claimId}) -> session summary / stored outcome
claim_sessions() -> active and unresolved summaries
claim_acknowledge_unknown({claimId}) -> allow explicit manual retry only
```

事件 `claim://result`、`claim://cancelled`、`captcha://failed`、`captcha://interactive` 均携带 `claimId`。领取结果另带 `accountId`、`revision`、`planId`、显式状态及现有业务字段；不包含 Token 或验证码参数。

- 前端必须先完成 Tauri listener 注册，再允许操作；每次先注册本次 waiter，再调用 `claim_start`。
- 使用 `captcha-{claimId}` 唯一窗口标签，仍最多一个活动验证流程。窗口 close 回调捕获创建时的 claimId，不通过复用窗口的当前 URL 推断旧会话。
- `src-tauri/capabilities/captcha.json` 将窗口匹配改为 `captcha-*`，保留现有最小权限。后端只关闭该会话的窗口；旧结果不得关闭新会话窗口。
- URL 携带 claimId；后端提交入口校验有效会话和调用验证码窗口标签。窗口迟到加载或重试前先查询 session，已取消或提交中的会话不能再次初始化验证。
- 前端使用 `Map<claimId, attempt>`，waiter 只是其中可结束的等待者。attempt 同时捕获账号出现代次 `accountEpoch`：仅删除/重加或 revision 变化时更新，普通重命名和刷新不变。结果接受顺序是：识别已知会话→处理其最终状态→校验账号版本和出现代次后刷新对应行→最后按需完成 waiter，不能以 waiter 不存在直接丢掉结果。
- 同一结果由事件、命令返回或 `claim_status` 多次到达时只处理一次。旧会话可以更新自己的记录，不能结束新 waiter、重建已删除的账号行或把成功提示挂在新登录身份上。
- `claim_sessions()` 用于主窗口重新加载后恢复状态；内存完成记录最多保留 100 条、10 分钟，活跃与未确认记录不按此规则丢弃。

### 4.3 超时、重启与结果不明

保留目前手动 90 秒、批量 120 秒、自动 45 秒的前端等待期限，但期限到达后先查询 `claim_status`：

1. 已完成：消费保存结果，覆盖事件丢失的情况。
2. 尚未提交：取消该会话，按原策略结束或跳过。
3. 正在提交：显示确认中，结束这次前端等待，但保留后端互斥；批量和自动流程等待该提交收尾，不开始下一次领取。
4. 返回 Unknown：该项记为待确认，允许继续其他账号；自动领取不能因进入下一轮或冷却到期而重发同一身份、同一套餐。

为避免程序重启清空互斥后自动重复领取，增加最小提交记录 `Paths::store_dir()/claim-attempts.json`，复用 `store::atomic_write`，不写入账号快照或 ZCode live 文件：

- 只存 schemaVersion、claimId、账号 ID、revision、身份范围摘要、planId、阶段、时间和必要的脱敏业务结果；不保存 JWT、OAuth Token、验证码参数或完整服务端响应。
- 身份范围摘要基于渠道和已校验的 ZCode subject，不依赖易变的 access token。revision 用于 UI 回填，不能用于绕过待确认拦截。保持当前领取只依赖严格 ZCode JWT 的兼容性，不额外要求 Coding Plan/OAuth 凭证。
- 发送 POST 前先成功落盘 Submitting 标记；落盘失败则不提交。网络期间不持有文件锁。完成后写入最终状态；落盘失败保留保守的未确认标记。
- 重启发现 Submitting 标记时归为 Unknown；记录损坏或不受支持的 schema 必须明确提示并暂停自动领取，不把它当作空记录。
- 清理仅针对已完成记录；未确认记录不因 TTL、凭证更新或普通删除/重加自动消失。该设计不能承诺断电场景或服务端的 exactly-once。
- 待确认行提供“检查领取结果”，只调用额度和预览查询，不激活、不提交。界面可以展示现状，但不得仅凭预览仍有/没有套餐自动断言原 POST 结果。
- 现有接口无法确认时，显示“已核对，重新尝试”的显式操作，经说明结果不明的确认后仅允许这一次手动新会话；不会恢复该套餐的自动重试。新会话完成后按明确结果更新记录。无需新建任务中心或历史记录页。

## 5. 验证码准备和失败恢复

- 参考 PR，将 `window.AliyunCaptchaConfig` 放在加载 SDK 之前。
- 后端缓存成功且可用的公共验证码配置，TTL 5 分钟；缓存按配置 endpoint 区分，不混入账号凭证。失败、disabled、缺 sceneId 不缓存。
- 配置 GET 对连接/DNS/I/O 暂态失败最多重试一次，间隔 300ms；HTTP 401、429、业务拒绝、无效配置不重试。SDK 加载及实例初始化各设 12 秒超时。
- 配置缓存使用独立同步机制合并同时获取，不占账号存储锁或领取状态锁。
- SDK timeout 后的成功回调不得提交。重试使用新的文档/初始化代次，旧回调不能改变新页面状态。
- 明确区分 setup 失败与 interactive 验证：前者手动操作可在原窗口重试配置；后者继续现有人工验证交互。
- 批量/自动模式遇到 setup 失败，取消未提交会话、显示该账号失败并继续其他账号；真正需要人工验证时，自动模式继续采用现有跳过与冷却规则。
- 重试按钮只重试验证准备。已经进入 Submitting 的会话不能重新加载后再次提交；同一成功回调触发多次也只有一次 POST。

## 6. 实施任务

执行时先检查工作区并记录基线，按下列顺序实施。使用 @test-driven-development 和 @verification-before-completion；每个任务的回归先失败、后通过。提交只包含该任务文件，不执行 `git add .`。下列命令均从仓库根目录执行。

### Task 1：共享有界队列

**Files:**
- Create: `src/request-queue.js`
- Create: `tests/request-queue.test.mjs`

1. 写 20 个受控 promise 的测试：峰值为 3、完成后补位、失败释放槽位、同步抛错释放槽位、重复 key 共享 promise、手动提升、同级 FIFO、取消排队任务不执行且 promise 能结束。
2. 运行 `node --test tests/request-queue.test.mjs`，确认缺模块或行为断言失败。
3. 实现第 3.1 节接口。不要引入依赖、网络重试或任务持久化。
4. 重跑上述命令；全部通过，使用受控 promise 而非真实计时判断并发。
5. 提交：`feat(queue): add bounded shared request scheduling`。

核心测试示例：

```js
import test from "node:test";
import assert from "node:assert/strict";
import { createRequestQueue } from "../src/request-queue.js";

test("duplicate clicks share one queued request", async () => {
  const queue = createRequestQueue(1);
  let calls = 0;
  let release;
  const first = queue.enqueue("a", async () => {
    calls++;
    await new Promise(resolve => { release = resolve; });
    return "ok";
  }, { priority: "background" });
  const second = queue.enqueue("a", () => assert.fail("duplicate"),
    { priority: "manual" });
  assert.equal(first, second);
  await new Promise(setImmediate);
  release();
  assert.equal(await first, "ok");
  assert.equal(calls, 1);
});
```

### Task 2：将额度刷新接入队列

**Files:**
- Modify: `src/main.js`（`loadAcctQuota`、`applyState`、`invalidateQuotaFor`、`sweepTick`）
- Modify if required: `src/quota-state.js`（仅必要的 ticket 查询，不替换已有校验）
- Modify: `tests/quota-state.test.mjs`
- Create: `tests/ui-harness.mjs`
- Create: `tests/refresh.test.mjs`

1. 建立离线 harness，执行真实状态/调度函数并 mock IPC；不能把被测调度逻辑复制进 mock。覆盖 20 个到期账号三路排空、手动去重与提升、旧版本成功/失败丢弃、排队期间删除、删除重加同版本、重命名不失效。
2. 运行 `node --test tests/quota-state.test.mjs tests/refresh.test.mjs`，确认新增并发/共享 promise 用例失败。
3. 按第 3.1 节封装现有额度调用；`sweepTick` 从 `find` 改为固定列表并入队，任务完成才安排该账号下一次扫描。保留既有 5 分钟周期与 jitter。
4. 重跑测试，确认旧 6 项额度状态回归仍通过；旧任务 finally 不得删除新任务或推迟新版本的扫描时间。
5. 提交：`feat(quota): schedule refreshes without weakening account isolation`。

### Task 3：领取资格并发与后端版本检查

**Files:**
- Create: `src/claim-state.js`
- Create: `tests/claim-state.test.mjs`
- Modify: `src/main.js`、`tests/refresh.test.mjs`
- Modify: `src-tauri/src/lib.rs`（`claim_preview`、`claim_refresh`）
- Modify: `src-tauri/src/store.rs`、`src-tauri/src/store/tests.rs`
- Modify: `src-tauri/src/claim.rs`、`src-tauri/src/claim/tests.rs`

1. 写失败用例：预览先到/后到、资格刷新复用、激活失败但预览成功、单账号失败继续、准确进度、单账号重试不改整批冷却、后端准备前后版本变化、首次设备初始化只生成一次且首次未联网。
2. 运行 `node --test tests/claim-state.test.mjs tests/refresh.test.mjs` 与 `cargo test --manifest-path src-tauri/Cargo.toml --lib`，核对新增断言失败原因。
3. 实现第 3.2、3.3 节契约和 `spawn_blocking`；刷新冷却与服务端限流分别处理。领取相关 HTTP 使用可注入传输或本地服务器测试，不调用官方地址。
4. 重跑测试；用受控后端任务确认网络等待期间其他 IPC 能响应，短锁不会把不同账号的网络调用串行化。
5. 提交：`feat(claim): refresh eligibility concurrently with scoped results`。

### Task 4：后端领取会话、提交互斥与待确认记录

**Files:**
- Create: `src-tauri/src/claim_session.rs`
- Create: `src-tauri/src/claim_session/tests.rs`
- Modify: `src-tauri/src/lib.rs`（领取命令、注册、窗口生命周期）
- Modify: `src-tauri/src/claim.rs`、`src-tauri/src/claim/tests.rs`
- Modify: `src-tauri/src/store.rs`（记录路径与必要的短锁辅助方法）
- Modify: `src-tauri/capabilities/captcha.json`

1. 写状态转换测试：同时 start 仅一个被接受；重复 submit 只有一个发送许可；准备期间取消不晚开窗口；提交后取消不解锁；错误 claimId 无影响；worker 失败必收尾；完成记录可读取且幂等；提交标记写失败时 HTTP 计数为零。
2. 写临时目录恢复测试：Submitting 重启变 Unknown、换凭证仍拦同身份套餐、另一身份不误拦、坏记录不静默清空、记录不含凭证；运行 `cargo test --manifest-path src-tauri/Cargo.toml --lib claim_session`，确认失败。
3. 实现第 4 节后端契约、唯一窗口标签及提交记录。领取 HTTP 设置零重定向、无自动重试；保留严格 JWT 获取。只在有效终态释放活动领取互斥。
4. 运行 `cargo test --manifest-path src-tauri/Cargo.toml --lib claim`，再跑完整 Rust 回归，确认原有外账号 key/冲突 JWT 拒绝测试仍通过。
5. 提交：`fix(claim): retain submission ownership and recover uncertain outcomes`。

### Task 5：前端会话协调与迟到结果

**Files:**
- Create: `src/claim-attempts.js`（纯会话/事件状态与去重）
- Create: `tests/claim-attempts.test.mjs`
- Modify: `src/main.js`（所有领取入口、监听、超时、自动中止）
- Modify: `tests/refresh.test.mjs`、`tests/ui-harness.mjs`

1. 将已发现的超时后成功丢失写成失败测试，另覆盖结果早于 `claim_start` 返回、事件与状态查询重复、旧事件不能完成新 waiter、删除/换版本不回填、提交中暂停下一次领取、自动关闭后仍处理最终结果。
2. 运行 `node --test tests/claim-attempts.test.mjs tests/refresh.test.mjs`，确认目标回归失败。
3. 实现 Map 会话和统一 `performClaim`；manual/all/auto 只改变交互策略，不另写提交路径。超时依后端状态分流，刷新资格打断自动流程时等待正在提交的请求结束。
4. 重跑测试。关键断言必须同时包括“旧 waiter 已结束”“迟到成功仍刷新正确账号”“POST 总次数仍为 1”“新会话未被清理”。验证主窗口重新加载通过 `claim_sessions` 恢复。
5. 提交：`fix(ui): reconcile late claim results independently of waiters`。

### Task 6：验证码配置缓存与准备重试

**Files:**
- Modify: `src/captcha.js`、`captcha.html`
- Modify: `src-tauri/src/claim.rs`、`src-tauri/src/claim/tests.rs`
- Create: `tests/captcha.test.mjs`
- Modify: `tests/ui-harness.mjs`

1. 写失败测试：配置先于 SDK、配置缓存 TTL、暂态错误最多两次 GET、401/429 不重试、失败不缓存、SDK 挂起、初始化挂起、晚回调无效、重复 success 一次提交、页面重载查询到 Submitting 不提交。
2. 运行 `node --test tests/captcha.test.mjs` 和 `cargo test --manifest-path src-tauri/Cargo.toml --lib claim::tests`，确认失败。
3. 实现第 5 节行为。重试按钮只在 session 仍可验证时出现；配置缓存不能缓存或记录验证码 token。
4. 重跑测试，确认 setup 失败不会触发“需要人工验证”的长期冷却，真实 interactive 不被当成网络重试。
5. 提交：`fix(captcha): recover setup failures without replaying claims`。

### Task 7：界面反馈、中英文与离线浏览器验收

**Files:**
- Modify: `src/main.js`、`src/styles.css`
- Modify: `src/locales/zh.js`、`src/locales/en.js`、`src-tauri/src/i18n.rs`
- Create: `tests/claim-ui.py`
- Modify: `tests/quota-ui.py`（适配新增 IPC 返回形状，保留既有断言）

1. 在真实前端页面注入人工 11 账号与模拟事件，建立失败截图/断言：逐项进度、部分失败、单账号重试、等待确认、待确认检查、显式手动重试、批量继续。
2. 实现简洁行内反馈；不展示内部 revision/claimId。结果未知不能显示已失败或已成功；重试按钮按真实阶段启用。
3. 在 520px 和常用窗口宽度、中英文下检查按钮、错误换行及渠道徽标。仍使用现有页面布局。
4. 运行 `npm run check:i18n`、`npm run build`，启动本地 Vite 后运行 `python tests/quota-ui.py` 和 `python tests/claim-ui.py`。模拟测试禁止非本地网络；结束后只清理本次服务进程。
5. 提交：`feat(ui): show refresh failures and claim confirmation states`。

### Task 8：集成回归、原生验收记录与交付

**Files:**
- Create: `docs/testing/concurrent-refresh-claim-recovery.md`
- Modify: `README.md`、`README.en.md`（仅更新已实现的行为）
- Modify if needed: `.github/workflows/ci.yml`（现有 npm/Rust 测试入口已能发现新增测试时不改）

1. 执行以下完整检查并记录真实结果、提交号和平台：

```powershell
npm test
npm run check:i18n
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --lib
npm run tauri build -- --debug --no-bundle
git diff --check
```

2. 对离线 11/20 账号场景记录任务开始/结束、峰值并发、调用次数和完成顺序。性能判断用受控延迟；不要用一次线上时间声称固定倍数收益。
3. Windows 原生 WebView 验收：关闭/重开验证码窗口、SDK 失败后重试、关闭主窗口再恢复、旧窗口关闭不影响新窗口、提交中关闭仍能收到结果。没有验证能力时明确记为未完成，不能用 Node mock 替代。
4. 真实激活上报、验证码与领取具有副作用，只有用户明确指定允许的测试账号后才执行；否则记录为待线上验收。程序测试使用独立临时 home，默认关闭自动领取，不能碰用户实际账号或现有安装。
5. 更新记录，提交 `docs: document concurrent refresh and claim recovery validation`。本计划不包含升版、推送、GitHub Release 或运行安装器；后续发布以用户的发布指令和当时验证结果为准。

## 7. 完成标准与回退

全部满足后才称本期实现完成：

- 共享队列峰值不超过 3；单任务失败、取消或版本失效不会卡住队列。
- 额度隔离原有回归全部通过；没有恢复旧候选 Token、错误 Key 过滤或 live 回退。
- 账号删除重加、更新凭证、切换当前账号、乱序响应均不相互覆盖、不串号、不回填旧状态。
- 资格刷新没有 5 秒逐账号 sleep；实际领取仍全局串行，同一会话最多一次 POST。
- 迟到结果得到正确处理；超时、取消、Unknown 和明确失败可区分，重启不会自动重放未确认提交。
- 验证码失败可以恢复，setup 与 interactive 分流，旧回调无法提交或关闭新窗口。
- 中英文、渠道标识和最小窗口布局可用；测试文档区分离线、浏览器、原生及线上验收。

分阶段检查点：Task 1–3 完成后可单独审查并发刷新；Task 4–7 完成后审查领取恢复。阶段间不得把未完成的会话协议当作可发布版本。

出现兼容问题时先把共享队列 limit 调为 1 或恢复原刷新调度，不回滚额度隔离。后端会话、前端事件协议和验证码窗口命名必须成组回退；已经存在的 `claim-attempts.json` 未确认记录不能静默丢弃。若需退回不认识该记录的旧安装包，先关闭自动领取并告知其不能恢复新版本的待确认状态。
