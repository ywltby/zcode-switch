# 并发刷新与领取恢复验收记录

- 日期：2026-10-26
- 平台：Windows（x64），PowerShell 7
- Node：v24.14.0；npm：11.12.0
- Rust：cargo 1.99.0 / rustc 1.99.0
- Python：3.14.3；Playwright 1.60.0（Chromium headless）
- 工作基线：`d27553f`（1.7.0-beta.4）
- 本文记录的实施提交：`48bd0a9` … `a4ea044`（详见 `git log`）

本记录区分离线测试、浏览器验收、Windows 原生 WebView 验收和线上副作用验收。**离线与浏览器通过不代表真实账号领取已通过验收。**

最新的查询/领取解耦实现与验证见第 8 节，beta.6 本机打包记录见第 9 节。此前各节保留当时版本的记录；新实现中关闭自动领取只关闭定时扫描，GUI 手动资格查询发现可领后也会提交后台领取，不能用关闭开关来保证 GUI 查询不领取。

## 1. 自动化检查

以下命令均在仓库根目录执行，结果为本机实测：

| 命令 | 结果 |
| --- | --- |
| `npm test` | 77 通过，0 失败（`node --test tests/*.test.mjs`） |
| `npm run check:i18n` | 通过（zh/en 各 241 个键） |
| `npm run build` | 通过（Vite 生产构建） |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 76 通过，0 失败 |
| `npm run tauri build -- --debug --no-bundle` | 成功，产物 `src-tauri/target/debug/zcode-switch.exe` |
| `git diff --check` | 无输出（退出码 0） |

新增或扩充的测试：

- `tests/request-queue.test.mjs`：峰值并发、失败/同步抛错释放槽位、重复 key 共享 promise、手动提升、同级 FIFO、取消排队任务。
- `tests/refresh.test.mjs`：20 个到期账号三路排空、手动去重与提升、旧版本成功/失败丢弃、排队期间删除、删除重加同版本、重命名不失效、失败释放槽位、旧任务不推迟新版本扫描、资格刷新与预览复用/先后顺序、`scope_changed` 只重试一次、领取完成后使提交前的预览失效、额度与领取共享同一队列（混合峰值不超过 3）、批量资格刷新三路并发、连续两次 `scope_changed` 后不残留 busy。
- `tests/claim-state.test.mjs` / `tests/claim-attempts.test.mjs` / `tests/claim-run.test.mjs`：资格 ticket 校验、迟到结果、waiter 与结果解耦、结果早于 `claim_start` 返回、提交中暂停下一次领取、事件与状态查询去重、后端取消事件结束对应 waiter。
- `tests/captcha.test.mjs`：配置先于 SDK、SDK/初始化超时、晚回调不提交、重复 success 只提交一次、重载发现 Submitting 不再初始化、setup 与 interactive 分流。
- Rust：`claim_session`（唯一活动会话、单次提交许可、准备期取消、提交后不解锁、仅成功清除旧 Unknown、任何失败保留旧记录、重启 Submitting → Unknown、坏记录不静默清空、记录不含凭证、待确认只放行一次已确认的手动重试、确认不会解除自动/批量拦截、渠道由唯一凭证推导时摘要稳定、数值与字符串型 JWT 主体不误合并）、`store` 领取快照版本校验与设备 ID 只初始化一次、`claim` 验证码配置缓存（TTL、按 endpoint、暂态重试一次、401/429/业务拒绝不重试、不可用配置不缓存）。

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

**部分完成（2026-10-26）。** 已在本机交互式桌面运行调试版，完成单账号显示、520px 窄布局、关闭主窗口到托盘及托盘恢复。验证码会话、提交中关窗、超时分流和待确认恢复仍未完成；Node 测试不能替代。

先从当前 HEAD 重新构建调试版（不生成安装包），并记录产物时间与提交号：

```powershell
npm run tauri build -- --debug --no-bundle
Get-Item .\src-tauri\target\debug\zcode-switch.exe | Select-Object FullName, LastWriteTime
git rev-parse --short HEAD
```

产物 `LastWriteTime` 必须晚于当前提交时间；若早于最新修复，重新构建后再验收。

准备独立 home、独立 ZCode 数据根与强制关闭自动领取（`$nativeCheckHome` 避免与只读变量 `$HOME` 冲突）：

```powershell
$nativeCheckHome = Join-Path $env:TEMP "zswitch-native-check"
$nativeDataRoot = Join-Path $nativeCheckHome "zcode-data"
$accountsDir = Join-Path $nativeCheckHome ".zcode-switch\accounts"
New-Item -ItemType Directory -Force $nativeCheckHome | Out-Null
New-Item -ItemType Directory -Force $accountsDir | Out-Null
New-Item -ItemType Directory -Force $nativeDataRoot | Out-Null
if (Test-Path "$env:USERPROFILE\.zcode-switch\accounts") {
  Copy-Item "$env:USERPROFILE\.zcode-switch\accounts\*" $accountsDir -Recurse -Force
}

$settingsPath = Join-Path $nativeCheckHome ".zcode-switch\settings.json"
if (Test-Path "$env:USERPROFILE\.zcode-switch\settings.json") {
  Copy-Item "$env:USERPROFILE\.zcode-switch\settings.json" $settingsPath -Force
}
if (Test-Path $settingsPath) {
  $settings = Get-Content $settingsPath -Raw | ConvertFrom-Json
} else {
  $settings = [pscustomobject]@{}
}
$settings | Add-Member -NotePropertyName auto_claim -NotePropertyValue $false -Force
$settings | ConvertTo-Json -Depth 8 | Set-Content $settingsPath -Encoding UTF8
if ((Get-Content $settingsPath -Raw | ConvertFrom-Json).auto_claim -ne $false) {
  throw "auto_claim must be false before native verification"
}

$env:ZCODE_SWITCH_HOME = $nativeCheckHome
$env:ZCODE_SWITCH_DATA_ROOT = $nativeDataRoot
# enc:v1 凭证的默认解密密钥包含原始 home；仅在测试进程中沿用来源密钥。
# 已配置显式密钥时保留原值，不输出密钥或解密后的凭证。
if (-not (Test-Path Env:ZCODE_CREDENTIAL_SECRET)) {
  $env:ZCODE_CREDENTIAL_SECRET = "zcode-credential-fallback:win32:$($env:USERPROFILE):$($env:USERNAME)"
}
& .\src-tauri\target\debug\zcode-switch.exe
```

`ZCODE_SWITCH_HOME` 承载账号库、设置与 `claim-attempts.json`；`ZCODE_SWITCH_DATA_ROOT` 承载 ZCode live 配置与凭证。两者都指向临时目录，验收期间不会读写真实安装或真实 ZCode 数据目录。临时数据根为空时，切换账号会在其中生成 live 配置；不要在真实 home 上执行。

检查项：

- 验证码窗口标签为 `captcha-<claimId>`；关闭后重开不会复用旧会话。
- SDK 失败时显示“重试验证准备”，点击只重试准备，不提交。
- 关闭主窗口到托盘再打开，待确认行仍存在（`claim_sessions` 恢复）。
- 旧验证码窗口关闭不影响新会话窗口；`capabilities/captcha.json` 匹配 `captcha-*`。
- 提交中关闭验证码窗口：主窗口收到结果或显示“已提交，正在确认结果”，后端互斥保持。
- 手动等待超时后按 `claim_status` 分流：已完成消费结果、未提交取消、提交中确认中、未知待确认。
- `claim-attempts.json` 不含 JWT、验证码参数或完整响应。

产物：原生窗口截图、时间线、脱敏后的 `claim-attempts.json`、执行人/日期、EXE 构建时间与提交号。

本轮实测（Codex，2026-10-26 16:49—16:53，用户已手动退出安装版）：

- 使用 `008ff7e` 源码构建的调试 EXE（构建时间 14:23:54，验收时 HEAD `ccb63fc` 仅新增文档），独立 home 为 `%TEMP%\zswitch-native-check-ba258170`；只复制一个已保存账号，`auto_claim=false`。
- 通过原生窗口截图与 Windows 无障碍接口验证：账号显示正常；520px 内容宽度下未见横向溢出；点击关闭后窗口隐藏且进程仍在；点击托盘图标后同一窗口恢复。此项不包含待确认记录恢复。
- 资格刷新暴露提示计数错误：无可领套餐的成功响应也计入“有可领套餐”。已修正为仅统计非空套餐，并在 `tests/claim-ui.py` 增加回归断言：11 个账号中一个无套餐、一个失败，应显示 9 个可领，而非 10 个。
- 截图保存在 `src-tauri/target/native-ui/2026-10-26/`：`native-account.png`、`refresh-count-bug.png`、`narrow-network-error.png`、`tray-restored.png`。截图记录的是修复前的原生版本；计数修复另经离线 UI 测试验证。
- 计数修复后：新增断言先复现“预期 9、实际 10”，修复后 `python tests/claim-ui.py` PASS；`npm test` 77 通过、`npm run check:i18n` 通过、`npm run tauri build -- --debug --no-bundle` 成功（含 Vite 构建，EXE 时间 16:56:21；基于 `ccb63fc` 加本轮未提交改动），未生成安装包。
- 验收后已从托盘菜单正常退出测试实例。未打开验证码窗口、未提交领取；没有生成 `claim-attempts.json`，因此本轮不将记录脱敏检查标为通过。

## 5. 线上副作用验收

**最新状态（2026-10-26 23:01）：单账号真实领取成功，异常路径仍未完成。** 用户晚间确认活动已可领取并要求立即测试，恢复本机已保存账号的单账号验收。下午曾因没有活动暂缓；该阶段额度专项结果见 [额度账号隔离验收记录](quota-account-isolation.md) 文末。

**下午执行记录（当时未完成真实领取）。** 用户已指示直接使用本机保存账号开展单账号验收。本轮选择 `blcj114514`（ID `42e94c6f-1d83-4510-ae48-561b2e3f92c1`），使用独立临时 home 与临时数据根，强制 `auto_claim=false`。本地副本隔离不等于线上账号隔离。

本轮结果：凭证未过期；首次 CLI 预览成功但 `plans=[]`。随后在 GUI 执行一次该账号的资格刷新（包含激活上报尝试），未出现可领取套餐。后续 CLI 预览遇到 `zcode.z.ai` DNS 解析失败（Windows 11001），界面也显示网络连接失败。本轮没有调用真实领取、没有重试提交、没有扩大到其他账号；原账号文件 SHA-256 与复制前一致。线上验收仍未通过，后续需在网络正常且账号确有可领套餐时继续。

**晚间实际领取（22:57—23:01，Codex 执行）：**

- 使用同一账号的新副本，独立 home 为 `%TEMP%\zswitch-live-claim-8066a20d`；自动领取关闭。EXE 构建时间 17:57:30，源码为 `ccb63fc` 加当前未提交改动。
- CLI 预览返回 `ZCODE GLOBAL BUILD`（`zcode-v3-start-plan-nt-1026`），奖励为 `GLM-5.3-Flash` 300,000,000 Token。GUI 刷新该账号资格后，点击行内“领取”一次，没有使用“全部领取”。
- 22:58:47 观察到原生“安全验证”窗口；22:58:50.526 落盘状态为 `succeeded`，`result.ok=true`，窗口随后自动关闭。claimId 为 `96403047-62df-4e6b-9f11-e128fd5da9e5`。验证码窗口结束较快，未取得其截图，不据此宣称其他验证码路径已通过。
- 领取后的 CLI 预览为 `plans=[]`。额度查询 `complete`、无错误，确认已领取套餐，剩余与总量均为 300,000,000 Token，有效期至 2026-10-27 09:00（UTC+08:00）。
- 首次自动刷新只显示已领取套餐，未显示额度条目；后续 CLI 查询及 23:01:02 手动刷新后，原生界面显示 `100%` 和 `3亿/3亿`。记录这一观察，不把首次额度条目缺失直接归因为前端或服务端。
- `claim-attempts.json` 仅一条 `succeeded`，无 `submitting/unknown`，没有重试。字段白名单检查通过，不含 JWT、加密凭证、验证码参数或完整接口响应；原账号文件与副本 SHA-256 均未改变。验收后从托盘正常退出测试实例。
- 本轮仅确认一个账号的一次成功路径。提交中关窗、失败/Unknown、确认重试和重启恢复等异常路径仍待验收，不扩展到批量或自动领取。

证据：`src-tauri/target/claim-live/2026-10-26/` 下的 `claimable-before.png`、`claimed-plan.png`、`quota-after-claim.png`、`summary.json` 与 `claim-attempts.redacted.json`。脱敏副本替换账号标识、revision 和身份摘要，不修改原落盘记录。

执行前需要确认：账号 ID、当前可领套餐、是否允许激活上报、是否允许一次真实领取。只做单账号手动领取，不做批量、不开自动领取。

步骤：

1. 预览确认（不触发激活上报）：在临时 home 下运行 `zcode-switch.exe --cli claim-preview --id <账号id>`。该命令会确保账号拥有设备 ID，可能初始化 `virtual_device_mid` 并写回**临时副本**的账号文件，因此不是无副作用的只读命令；不得对真实账号库执行。
2. GUI 单账号领取，记录 claimId、开始/结束时间、最终状态。
3. 成功：额度与可领套餐在同一行刷新，`claim_sessions` 无 unknown。
4. 失败或 Unknown：界面区分明确失败与待确认；重启后同身份同套餐的自动领取仍被拦截；“检查领取结果”只调用额度与预览。
5. 显式重试：确认后只允许一次；重试失败（含验证码 3007）仍保留旧待确认记录。
6. 首次领取触发设备 ID 初始化时返回 `scope_changed`，刷新 state 后自动重试一次。

一次线上结果不能外推为所有账号通过；发布验收仍需单独记录。

## 6. 已知边界

- 应用层并发上限为 3，不覆盖 OAuth 等其它功能的 HTTP 请求。
- `claimId` 只用于本地会话关联，不假设服务端提供幂等语义；断电或服务端 exactly-once 无法承诺。
- 提交记录只保存脱敏字段；不保存 JWT、OAuth Token、验证码参数或完整服务端响应。
- `claim-attempts.json` 损坏时自动领取暂停并提示，不把坏记录当作空记录；退回不认识该记录的旧版本前应先关闭自动领取。
- 新请求失败不能推断旧请求结果：待确认记录只能由同一身份、同一套餐的新请求**成功**清除。

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
| 手动重试在预览阶段失败，却删除了旧 Unknown，自动领取被误解锁 | 只有**成功**才清除旧记录；准备失败、提交后失败、验证码/业务拒绝（含 3007）与提交标记落盘后未发 POST 的本地失败都保留旧记录 | `a_pre_submit_failure_keeps_the_unknown_it_replaced`、`a_local_auth_failure_after_the_marker_keeps_the_old_unknown`、`a_captcha_rejection_of_the_retry_keeps_the_old_unknown` |
| 缺少 `oauth:active_provider` 时身份摘要使用 `unknown`，补齐字段后同一账号摘要变化 | 身份摘要改用 `resolve_account_context` 中已验证的渠道与主体 | `identity_scope_matches_a_uniquely_inferred_provider` |
| 准备期间取消后仍打开验证码窗口 | `mark_verifying` 失败即放弃开窗并返回“领取已取消” | `cancel_while_preparing_blocks_late_verification`；命令路径增加显式判断 |
| 资格刷新成功但无套餐，仍被计入“有可领套餐” | 仅在成功响应的 `plans` 非空时增加可领账号数 | `tests/claim-ui.py`：11 个账号中一个无套餐、一个失败，提示可领数为 9 |

## 8. 查询与后台串行领取解耦（2026-10-27）

**状态：代码与离线验收完成；新队列的原生及线上领取验收未执行。** 基于 `ccb63fc` 加当前未提交改动；保留此前 beta.5 的额度修复、版本元数据和已生成安装包。

用户明确要求手动与自动查询均在查到可领时立即领取，不受自动开关限制。本次实现：

- 资格和额度查询继续共享 3 个并发槽位；每个账号的有效资格结果立即进入独立领取队列，不等待全部扫描结束。领取只运行一个任务，沿用 5 秒间隔、失败冷却及每账号上限。
- 自动领取开关只控制每 10 分钟的定时扫描；开启后立即扫描一次。关闭不取消已排队任务；领取进行中仍允许扫描、单账号查询重试和切换定时扫描开关。
- 按账号代际及套餐去重；资格消失、账号删除、删除后重新添加或登录凭证改变，撤销尚未开始的旧任务。查询失败保留显示，但不发布新的可领结果。
- 待确认账号不自动重试；后端仍在提交时暂停后续领取，扫描继续。超时后每 5 秒读取后端状态，终态通过原有 claimId 处理一次，随后继续领取；缺失状态或网络失败不会冒充提交结束。
- 卡片显示“排队中”或“领取中”。自动开关与手动刷新提示均说明查询后会后台领取。

验证（全为本机实测）：

| 命令 | 结果 |
| --- | --- |
| `npm test` | 97 通过，0 失败；包含查询逐项发布、串行队列、代际隔离、去重、冷却及超时回执恢复 |
| `npm run check:i18n` | 通过，zh/en 各 243 个键 |
| `python tests/claim-queue-ui.py` | PASS：自动关闭时手动刷新即入队；首个领取悬挂时 11 账号扫描完成，查询峰值 3、领取峰值 1；重复扫描、开关切换、删除/换登录、待确认保护及 520px 布局 |
| `python tests/claim-ui.py` | PASS：会话恢复、单账号重试、待确认检查、显式重试、批量继续、英文及窄屏 |
| `python tests/quota-ui.py` | PASS：额度归属、缓存、旧响应隔离、删除、切换及窄屏 |
| `npm run tauri build -- --debug --no-bundle` | 成功（含 Vite 生产构建）；EXE 为 `src-tauri/target/debug/zcode-switch.exe`，LastWriteTime 2026-10-27 00:38:45 |
| `git diff --check` | 通过；仅工作区换行格式提示 |

队列截图：`src-tauri/target/claim-queue-ui/scanning-and-queue.png`。三个 UI 测试只使用合成账号和模拟 IPC，并拦截外部网络。此次未启动真实领取、未修改本机账号/自动领取设置、未安装或重新打包。既有 beta.5 安装包不包含本节的新队列实现；历史单账号真实领取成功不能作为新队列的线上验收结果。

## 9. beta.6 本机打包（2026-10-27）

用户随后要求新 beta，并明确选择“只生成本机安装包”。版本升至 `1.7.0-beta.6`，包含第 8 节全部改动；同步 package、npm lock、Cargo、Cargo lock 和 Tauri 配置，中英文 README 已更新开关含义。未提交、推送或上传 GitHub Releases，未替换当前安装实例。

- 本轮重新执行 `npm test`：97 通过；`cargo test --manifest-path src-tauri/Cargo.toml --lib --locked`：76 通过；`npm run check:i18n`：243 个键通过。
- `npm run dist` 成功（含生产前端、Rust release 与 NSIS 构建），安装包和便携版生成时间均为 2026-10-27 01:17:29。Windows 文件版本、产品版本均为 `1.7.0-beta.6`。
- 两个分发文件的 SHA-256 与各自构建产物一致。便携版在独立的空账号 home/data root 中执行 `--cli list`，退出码 0，返回 `ok=true, accounts=[]`；没有启动 GUI 或真实领取。
- beta.5 两个文件的 SHA-256 与原校验文件一致，保留原件。测试证据：`src-tauri/target/release-check-beta6/verification.json`。

| 本机文件（`release/`） | 字节数 | SHA-256 |
| --- | ---: | --- |
| `zcode-switch_1.7.0-beta.6_x64-setup.exe` | 3,445,131 | `81a4d39b3ad4bccb42c3de90af01871e42255389b8aa8ca831de1fe083d114ef` |
| `zcode-switch_1.7.0-beta.6_portable.exe` | 14,424,576 | `67f35568a8798ddd9fc231f5de5054be7809254fbaa989cf749b1dd72fa7e709` |

校验文件：`release/checksums-Windows-v1.7.0-beta.6.txt`；[版本说明](../releases/v1.7.0-beta.6.md)。本轮是本机 beta 打包，不将第 8 节尚未执行的新队列原生/线上领取验收标记为通过。


## 10. 单账号资格查询

账号行在查额度按钮左侧增加“查资格”，复用既有 `refreshClaimOne` 和 `claimFlow.refresh(id)`，只对选中的账号尝试活动激活上报并查询资格。有效结果进入与批量刷新相同的后台串行领取队列，自动领取开关关闭时仍然如此；已有的去重、待确认保护和冷却规则不变。

按钮在本账号查询期间禁用并显示查询状态；成功但无可领套餐、查询失败以及账号状态变化都会提示。离线测试 `tests/claim-single-ui.py` 使用模拟账号和 IPC，覆盖单账号请求、同时查询其他账号、关闭自动扫描时入队、领取进行中继续查询、重复查询去重、失败与状态变化反馈、英文与 520px 布局；不访问真实 API，也不提交真实领取。额度、滚动、领取队列和领取界面的既有离线回归继续通过。

beta.10 安装包不包含此按钮；此变更已包含在下节的 beta.11 本机安装包中。

## 11. beta.11 本机打包（2026-10-28）

版本包含单账号查资格、自动验证回调时序保护、过期登录切换保护、备份设备标识保留，以及切换时保留 ZCode 本机设备与监控状态。每账号原有领取标识和并发查询、后台串行领取规则不变。基于 `ccb63fc` 加当前未提交改动构建；只生成本机文件，未提交、推送、上传或安装。

- `npm test` 100 通过，`cargo test --lib --locked` 89 通过，`check:i18n` 中英文各 254 个键通过。
- 验证码、单账号资格查询、后台队列的离线 UI 检查通过；对打包生成的生产前端另行检查 520px 布局、自动开关关闭时的单账号入队及验证码回调。所有 UI 检查均拦截外网并使用模拟 IPC。
- Windows release 便携版在隔离 home/data root 执行 8 项合成数据检查，通过过期账号拦截、已激活账号更新、冷切换原生状态保留、无原生文件时不创建以及单账号/多账号加密备份往返。Rust 模拟进程测试覆盖热切换及重新登录后保留账号设备标识。
- 安装包和便携版的文件与产品版本均为 `1.7.0-beta.11`，分别与构建产物的 SHA-256 一致。未操作真实账号、真实领取或运行中的 ZCode；上述离线检查不等同于原生及线上领取验收。

| 本机文件（`release/`） | 字节数 | SHA-256 |
| --- | ---: | --- |
| `zcode-switch_1.7.0-beta.11_x64-setup.exe` | 3,442,587 | `34ac9f5c01876f70e51312007bd2753c3fbf22b1154aed30313e143e928d5b0e` |
| `zcode-switch_1.7.0-beta.11_portable.exe` | 14,422,016 | `c68d71324dfb6ea202fa59b302b402d0fd4c9c77ad804895b094d7300a7130bd` |

校验文件：`release/checksums-Windows-v1.7.0-beta.11.txt`；[版本说明](../releases/v1.7.0-beta.11.md)。
