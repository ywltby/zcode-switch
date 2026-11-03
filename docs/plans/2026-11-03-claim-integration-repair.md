# 领取链路整合修复计划

> 执行本计划时，按任务逐项实现和验证；先复现问题，再修复。未经用户要求，不提交、推送或发布。

**目标：** 保留 `wjs/main` 的领取协议和活动功能，恢复本仓库已有的账号隔离、切换安全、领取恢复与界面功能，修复本次审核发现的整合问题。

**整合方式：** 以 `70b133f` 为原有功能基准，以 `ada2795`（`wjs/main`）为领取和活动实现的对照，基于当前工作区增量修改。领取协议使用 wjs 的 preview、marketing/touch、billing/current、claim 和活动元数据实现；严格 JWT、账号代次、持久化会话与安全切换作为必要保护接入，不整文件覆盖任一版本。

**技术栈：** Rust、Tauri 2、JavaScript、Node test runner、Python Playwright。

## 开始前

1. 运行 `git status --short --branch`、`git diff`、`git diff --cached` 和 `git ls-files --others --exclude-standard`，记录已有暂存、未暂存和未跟踪文件。当前 HEAD 为 `0b2344f`；若状态变化，以实际状态为准。
2. 对照 `git diff ada2795 -- src-tauri/src/claim.rs` 和 `git diff 70b133f -- src-tauri/src`，确认 wjs 领取实现及本仓库修复各自的依赖。不得 reset、clean、整文件覆盖或改变已有暂存状态。
3. 保留现有明确约定：手动查到资格立即入队；自动领取开关只控制定时扫描，关闭不取消已排队任务。查询和领取仍分离、领取串行。入队后的时间判断、已领取防重和风控必须生效。
4. 测试只使用临时 home、合成账号、模拟凭证和本地 mock。禁止真实上游请求、真实领取、改动真实 ZCode 数据或运行安装器；活动激活上报也必须旁路到 mock 或使用注入传输。

## 任务 1：恢复当前登录的真实后端路径

**涉及：** `src-tauri/src/lib.rs`、`src-tauri/src/store.rs`、`src/main.js`、相关 Rust 和 UI 测试。

1. 增加后端回归：构造未保存的合成当前登录，断言 `live` 的资格查询、刷新、领取快照和结果查询读取 live 凭证及 live 设备 ID，不读取 `accounts/live.json`，不创建或改写真实/模拟客户端设备文件。
2. 覆盖请求前后切换 live 登录的情况：旧结果必须因身份或 revision 变化被拒收；迟到领取结果不能刷新成另一个登录的状态。
3. 恢复 `claim_snapshot`、`claim_revision_ok` 对 `LIVE_ID` 的独立处理，并检查 `claim_current`、会话结果回填及活动展示对 `live` 的支持。CLI 已有独立 live 路径，不能以 CLI 结果代替 GUI 后端验证。
4. 先运行新测试确认旧代码失败，完成后重跑通过。对 UI 边界的 mock 之外，还要验证实际后端路径；无法完成原生 WebView 验证时明确说明。

## 任务 2：统一代理和版本上报

**涉及：** `src-tauri/src/net.rs`、`quota_http.rs`、`claim.rs`、`quota.rs`、`cli.rs`、`lib.rs`、mock 测试。

1. 使用本地代理和本地目标服务建立两组测试：GUI 初始化及修改配置后，额度、领取、活动查询和激活请求采用同一代理；CLI 的对应请求也采用同一代理。验证代理关闭时的行为；不得使用真实服务。
2. 消除 `net.rs` 和 `quota_http.rs` 两个互不相通的代理状态，保留一个明确的数据源，并覆盖进程启动和运行中设置变更。
3. 恢复 wjs 对活动请求的版本保鲜：按数值比较已检测版本、内置领取版本和 changelog 缓存，选择最高的有效值；无缓存、旧缓存、探测失败和较新本机版本都要测试。用本地 mock 验证 URL 参数及请求头实际带出的版本。
4. 对照本仓库之前对 OAuth 版本的修复；如 OAuth 需要独立固定版本，将 OAuth 版本与领取/额度版本分开，避免修复活动版本时改变登录协议。

## 任务 3：自动领取准入、去重和结果回填

**涉及：** `src/main.js`、`src/claim-queue.js`、`src/claim-flow.js`、`tests/claim-queue.test.mjs`、浏览器测试。

1. 用受控时钟和合成活动先复现：未来活动、已过期活动及已经成功领取的同账号同套餐，不得被自动领取消费者提交；不同账号或不同套餐不能错误合并。
2. 覆盖本地已领取标记、领取成功后服务端短期仍返回旧资格、10 分钟冷却后再扫描、程序重新加载及迟到成功等场景。成功结果应在会话归属仍有效时持久标记，并更新已领取状态；不能仅依赖队列的短期 `recent`。
3. 将 wjs 的开始/结束时间、已领取状态与风控准入接入所有自动消费路径，包括并发扫描产生的队列和到点任务。保留“手动查资格立即入队”的既有约定；未来活动不能因此提前提交。
4. 继续验证：旧账号代次、删除重加、资格撤销和查询失败不会触发旧任务；已提交或未知结果不会自动重复提交；同一时间只有一个领取会话。

## 任务 4：领取结果分类与风控

**涉及：** `src-tauri/src/claim.rs`、`src-tauri/src/lib.rs`、`src/main.js`、`src-tauri/tests/mock_upstream.rs`、`src-tauri/src/claim/tests.rs`。

1. 补充真实 `submit_claim` 本地 mock 回归：POST 返回有效 JSON 但缺少或无法识别业务码时为 `unknown`；传输、响应读取和 worker 异常为 `unknown`；明确业务拒绝为 `failed`；成功仍为 `succeeded`。
2. 覆盖 HTTP 401/403 返回业务码及验证码挑战的场景，保留业务 `code`、`terminal`、`challenge`、`nextAt`；1005 应保留上游时间，3007 不自动换码重试，验证码挑战不误判为登录失效。
3. 将上述字段完整传到事件和会话持久化结果，并验证 UI 提示、终态不重试和冷却时长。不能只测试 `submit_claim` 而漏测 Tauri 事件载荷和队列。
4. 覆盖领取准备中 preview 返回 3012 / unusual activity：记录冷却后必须中止当前准备，不打开验证码，也不发送 claim POST。对非风控的普通预览变化保留 beta.11“本地列表不决定最终资格”的既有设计，不能一概改为强依赖列表。

## 任务 5：损坏记录与提交恢复

**涉及：** `src-tauri/src/claim_session.rs`、`src-tauri/src/lib.rs`、`src/main.js`、会话和 UI 回归。

1. 构造损坏 JSON、无法读取、不支持的 schema 和未知 phase 的记录，验证恢复失败时原文件字节不被自动覆盖，也不能被重写成空记录。
2. 验证恢复失败时，自动、到点、批量和普通手动领取均不能绕过保护并覆盖记录；界面提供明确状态。需要恢复记录时不得自动删除或静默丢弃。
3. 验证提交标记写入失败不发送 POST；提交中的取消不释放互斥；重启时提交中转为未知；未知结果需显式人工确认，失败重试不能清除旧未知记录，只有确定成功才能清除。

## 任务 6：恢复发布配置及复核其他改动

**涉及：** `.github/workflows/release.yml`、`.github/workflows/ci.yml`、`src-tauri/src/oauth.rs`、`store.rs`、`probe.rs` 及对照差异。

1. 以 `70b133f` 为基准恢复本仓库发布说明和带 `-` 标签的预发布判定，移除随 wjs 引入且不适用于本仓库的固定版本说明及发布身份设置；静态验证 beta、rc 和正式标签。
2. 比较非领取文件的实际差异，确认账号隔离、项目 Token、过期切换拦截、原生设备状态、备份设备 ID 和登录功能没有被误覆盖。对有意保留的其他差异列出理由，不能整文件回滚。
3. 把会话、mock、原生切换和适合 CI 的 UI 回归纳入相应平台检查；确认新增测试文件实际会被提交，不能只靠本地未跟踪文件通过。

## 最终验证

依次执行并记录命令、结果和失败原因，不以测试数量代替覆盖分析：

```powershell
npm test
npm run check:i18n
npm run build
cargo test --manifest-path src-tauri/Cargo.toml --lib -- --test-threads=1
cargo test --manifest-path src-tauri/Cargo.toml --test mock_upstream -- --test-threads=1
cargo build --manifest-path src-tauri/Cargo.toml
python tests/native-switch-safety.py
```

使用确认指向本仓库的本地 Vite 服务，依次运行 `tests/quota-ui.py`、`claim-ui.py`、`claim-queue-ui.py`、`claim-single-ui.py`、`captcha-ui.py`、`scroll-ui.py` 和新增场景。不要结束不属于本次验证的已有服务。检查 `git diff --check` 与最终 `git status --short --branch`，确保用户已有暂存/未暂存内容未被覆盖。

最终报告逐项说明：对照了 wjs 的哪些领取实现、保留了哪些本仓库修复、每项回归的复现与通过证据、各命令的实际结果、未完成的原生/真实上游及跨平台验证。不能因为 mock 和单元测试通过，就声称真实领取或远端 CI 已验证。
