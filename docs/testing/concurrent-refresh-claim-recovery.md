# 并发刷新与领取恢复验收记录

- 日期：2026-10-26
- 平台：Windows（x64），PowerShell 7
- Node：v24.14.0；npm：11.12.0
- Rust：cargo 1.99.0 / rustc 1.99.0
- Python：3.14.3；Playwright 1.60.0（Chromium headless）
- 工作基线：`d27553f`（1.7.0-beta.4）
- 本文记录的实施提交：`48bd0a9` … `a4ea044`（详见 `git log`）

本记录区分离线测试、浏览器验收、Windows 原生 WebView 验收和线上副作用验收。**离线与浏览器通过不代表真实账号领取已通过验收。**

## 1. 自动化检查

以下命令均在仓库根目录执行，结果为本机实测：

| 命令 | 结果 |
| --- | --- |
| `npm test` | 77 通过，0 失败（`node --test tests/*.test.mjs`） |
| `npm run check:i18n` | 通过（zh/en 各 241 个键） |
| `npm run build` | 通过（Vite 生产构建） |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 75 通过，0 失败 |
| `npm run tauri build -- --debug --no-bundle` | 成功，产物 `src-tauri/target/debug/zcode-switch.exe` |
| `git diff --check` | 无输出（退出码 0） |

新增或扩充的测试：

- `tests/request-queue.test.mjs`：峰值并发、失败/同步抛错释放槽位、重复 key 共享 promise、手动提升、同级 FIFO、取消排队任务。
- `tests/refresh.test.mjs`：20 个到期账号三路排空、手动去重与提升、旧版本成功/失败丢弃、排队期间删除、删除重加同版本、重命名不失效、失败释放槽位、旧任务不推迟新版本扫描、资格刷新与预览复用/先后顺序、`scope_changed` 只重试一次、领取完成后使提交前的预览失效、额度与领取共享同一队列（混合峰值不超过 3）、批量资格刷新三路并发、连续两次 `scope_changed` 后不残留 busy。
- `tests/claim-state.test.mjs` / `tests/claim-attempts.test.mjs` / `tests/claim-run.test.mjs`：资格 ticket 校验、迟到结果、waiter 与结果解耦、结果早于 `claim_start` 返回、提交中暂停下一次领取、事件与状态查询去重、后端取消事件结束对应 waiter。
- `tests/captcha.test.mjs`：配置先于 SDK、SDK/初始化超时、晚回调不提交、重复 success 只提交一次、重载发现 Submitting 不再初始化、setup 与 interactive 分流。
- Rust：`claim_session`（唯一活动会话、单次提交许可、准备期取消、提交后不解锁、准备失败保留旧 Unknown、重启 Submitting → Unknown、坏记录不静默清空、记录不含凭证、待确认只放行一次已确认的手动重试、确认不会解除自动/批量拦截、明确结果替换旧记录、渠道由唯一凭证推导时摘要稳定、数值与字符串型 JWT 主体不误合并）、`store` 领取快照版本校验与设备 ID 只初始化一次、`claim` 验证码配置缓存（TTL、按 endpoint、暂态重试一次、401/429/业务拒绝不重试、不可用配置不缓存）。

## 2. 离线并发统计

用真实调度器（`src/refresh-scheduler.js`）与受控延迟（每请求 50ms）驱动，不是线上计时：

| 账号数 | 队列上限 | 峰值并发 | IPC 调用 | 总耗时 |
| --- | --- | --- | --- | --- |
| 11 | 3 | 3 | 11 | 261 ms |
| 20 | 3 | 3 | 20 | 430 ms |
| 11 | 1 | 1 | 11 | 677 ms |

轮次内完成顺序与入队顺序一致（`a0, a1, a2, a3, a4 …`）；总耗时只说明调度开销，不代表真实网络收益。资格刷新与额度共享同一队列，批量资格刷新并发发起、每批最多 3 个；浏览器验收中 11 个账号在约 1.2 秒内完成（每个账号 300ms 模拟延迟，3 路并发）。

## 3. 浏览器离线验收

本地 Vite（`http://127.0.0.1:5173`）+ Playwright Chromium，注入合成账号与模拟 Tauri 边界；非本地网络请求一律拦截。

- `python tests/quota-ui.py`：PASS。保留原有 A/B/C 额度断言：部分失败、失败保留旧值与时间戳、凭证更新丢弃旧响应、5 秒状态轮询、迟到成功/失败、删除、切换、520px 窄布局。
- `python tests/claim-ui.py`：PASS。11 个合成账号：
  - 启动恢复：未知结果（含 claimId `old-2`）与提交中会话分别显示“领取结果待确认”“已提交，正在确认结果”；提交中会话完成后自动解除。
  - 批量资格刷新：逐项进度 `刷新资格 n/11`（并发完成后计数）；单账号失败显示错误与“重试本账号”；11 次调用在 5 秒内完成；未触发 `claim_captcha_submit`。
  - 单账号重试：只新增一次该账号调用，整批冷却按钮保持禁用。
  - 待确认：显示“检查领取结果”，只调用额度与预览，不提交。
  - 显式重试：二次确认后调用 `claim_acknowledge_unknown`，使用新 claimId 重新领取。
  - 批量领取：第一个账号失败后仍继续第二个账号，结束后按钮恢复。
  - 页面不渲染内部 claimId 或 plan id。
  - 英文界面与 520px 视口无横向溢出。

截图输出目录：`src-tauri/target/claim-ui/`（`recovered.png`、`unconfirmed.png`、`narrow-en.png`）。

## 4. Windows 原生 WebView 验收

**未完成。** 本次执行环境没有可交互的桌面会话，未运行 `zcode-switch.exe` 做下列人工路径：

- 关闭/重开验证码窗口。
- SDK 失败后点击“重试验证准备”。
- 关闭主窗口（最小化到托盘）后重新打开并恢复待确认状态。
- 旧验证码窗口关闭不影响新会话窗口；窗口标签为 `captcha-<claimId>`，能力文件匹配 `captcha-*`。
- 提交中关闭验证码窗口后仍能收到 `claim://result`。

Node 测试不能替代这些路径；它们仍待在有桌面会话的 Windows 机器上验收。

## 5. 线上副作用验收

**未执行。** 真实激活上报、验证码与领取都会产生副作用，需要用户明确指定测试账号。默认设置未改动（自动领取仍默认关闭），测试使用独立临时 home，未触碰用户账号、ZCode live 文件或现有安装。

上线前仍需在用户指定的测试账号上确认：

- 首次领取触发设备 ID 初始化时返回 `scope_changed` 并在刷新 state 后自动重试一次。
- 提交成功后额度与可领套餐在同一行更新。
- 提交结果不明时进入“待确认”，重启程序后仍拦截同身份同套餐的自动领取，显式重试只允许一次。

## 6. 已知边界

- 应用层并发上限为 3，不覆盖 OAuth 等其它功能的 HTTP 请求。
- `claimId` 只用于本地会话关联，不假设服务端提供幂等语义；断电或服务端 exactly-once 无法承诺。
- 提交记录只保存脱敏字段；不保存 JWT、OAuth Token、验证码参数或完整服务端响应。
- `claim-attempts.json` 损坏时自动领取暂停并提示，不把坏记录当作空记录；退回不认识该记录的旧版本前应先关闭自动领取。

## 7. 复核修复记录

复核发现并修复了以下问题，均新增回归测试：

| 问题 | 修复 | 回归 |
| --- | --- | --- |
| 额度调度器忽略传入队列，内部另建一个，混合请求峰值 6 | `createQuotaScheduler` 接受并复用外部队列 | `quota and claim eligibility share the same three slots` |
| 批量资格刷新逐个 `await`，峰值只有 1 | `refreshMany` 并发发起，进度按完成计数 | `batch eligibility refresh runs three accounts at a time` |
| 待确认账号可被“一键领取”直接开新会话；确认会永久解除自动拦截 | 批量与自动始终被拦截；确认只发放一次性手动许可，明确结果替换旧记录 | `only_an_acknowledged_manual_claim_passes_the_unconfirmed_gate`、`a_definite_outcome_clears_the_unknown_it_replaced`；`tests/claim-ui.py` 断言待确认账号不进入批量 |
| 连续两次 `scope_changed` 后行内 busy 永久残留 | 任何未落结果的收尾都释放 ticket | `two consecutive scope_changed results never leave the row busy` |
| 后端 `claim://cancelled` 前端未监听，关窗后仍等待超时 | 新增监听并结束对应 waiter | `a backend cancellation ends its own attempt without a claim outcome`；`tests/claim-ui.py` 断言关窗后领取按钮立即恢复 |
| JWT 数值型主体被当作 `unknown`，同渠道不同账号共享拦截范围 | 身份摘要复用 `normalized_id`，数值与字符串主体等价 | `identity_scope_accepts_numeric_and_string_subjects_without_merging_accounts` |
| 手动重试在预览阶段失败，却删除了旧 Unknown，自动领取被误解锁 | 只有 `succeeded` 或**已发送 POST 后**的明确失败才替换旧记录；准备失败保留旧记录 | `a_pre_submit_failure_keeps_the_unknown_it_replaced`、`a_post_submit_rejection_does_supersede_the_replaced_unknown` |
| 缺少 `oauth:active_provider` 时身份摘要使用 `unknown`，补齐字段后同一账号摘要变化 | 身份摘要改用 `resolve_account_context` 中已验证的渠道与主体 | `identity_scope_matches_a_uniquely_inferred_provider` |
| 准备期间取消后仍打开验证码窗口 | `mark_verifying` 失败即放弃开窗并返回“领取已取消” | `cancel_while_preparing_blocks_late_verification`；命令路径增加显式判断 |
