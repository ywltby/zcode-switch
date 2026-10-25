# 额度账号隔离实施与验收记录

执行日期：2026-10-26（Windows）。对应[实施计划](../plans/2026-10-25-quota-account-isolation.md)，基线为 `f34225686dfef05d84c256a56f868719248f15ff`。

**当前状态：阶段 A/B 的代码、离线回归和 11 份账号副本的 CLI 实际接口检查已执行。官方 ZCode 界面数值对照、原生 Tauri 界面的真实账号验收，以及跨平台 CI 尚未完成，不能据此宣布全部验收通过。** 未替换已安装程序；远端 CI 进度以本提交对应的 GitHub Actions 运行记录为准。

## 实现范围

- 每次查询从指定快照建立不可变上下文，绑定账号、ZCode 身份、平台、登录代次、已有设备 ID 与版本。OAuth 业务身份和 ZCode 身份分别处理。
- Start Plan 只使用本账号 ZCode JWT；Coding Plan 通过官方接口发现项目，复用既有 Key ID，再 POST 签发短期项目 Token。历史 provider key 和未经验证的 location 缓存不参与鉴权。首版直接重新发现项目，不采用历史 location 提示。
- Token 仅保存在内存，账号、代次、平台、origin、业务主体、组织、项目、Key ID 与 usage scene 都参与隔离。同范围签发合并，不同账号和项目独立。
- 有效期取服务端绝对到期时间与请求开始时间加 TTL 的较小值，提前刷新并加入抖动。401 最多重签并重试一次；被拒绝的 Token 不再复用。网络/429 签发失败仅允许复用同范围、尚未过期且未被拒绝的 Token，不延长有效期。429 不自动重放请求；HTTP Retry-After 支持秒数和 HTTP 日期，限制为 1–300 秒，缺失或无效时为 5 秒。
- 合并前校验账号范围；不同项目、订阅实例和有效期分别保留。重复窗口不相加，先比较字段完整性，同等完整时选择较新的结果；无法证明是同一权益时保留独立分组。
- 明确区分 `complete`、`partial`、`unavailable`；成功确认无套餐才显示未开通。团队订阅已确认而后续 Key/Token/额度请求失败时，保留订阅信息。
- store 在请求前后检查登录代次，查询使用只读路径；网络等待不持有全局账号存储锁。CLI、probe 和 UI 共用服务，claim 只收紧 JWT 来源。
- UI 同时检查账号、revision 和请求序号。更新、删除或 5 秒轮询发现登录变化后丢弃旧成功/失败；同代次刷新失败保留旧结果及其原更新时间。

## 自动化验证

以下命令从仓库根目录执行。Rust/Node 回归使用人工 A/B/C 数据、临时目录和注入传输，不读取真实账号或请求官方接口。

| 命令/检查 | 本机结果 |
| --- | --- |
| `npm ci` | 通过；停止本次测试遗留的 Vite 进程后恢复依赖 |
| `cargo test --manifest-path src-tauri/Cargo.toml --lib` | 47 通过，0 失败 |
| `npm test` | 6 通过，0 失败 |
| `npm run check:i18n` | 中英文各 212 个键，通过 |
| `npm run build` | Vite 构建通过 |
| `cargo build --manifest-path src-tauri/Cargo.toml` | Windows debug 构建通过，CLI 已运行 |
| `npm run tauri build -- --debug --no-bundle` | 独立 Windows 程序构建通过，包含前端资源；未生成或运行安装器 |
| `npm run dist`（1.7.0-beta.2） | Windows release 构建及 NSIS 安装包通过，另产出便携版和 SHA-256 校验文件 |
| `git diff --check` | 通过；仅有已有换行配置的 LF/CRLF 提示 |

独立程序：`src-tauri/target/debug/zcode-switch.exe`。Tauri 构建保留了现有 bundle identifier 以 `.app` 结尾的 macOS 建议提示；本轮未改动该配置。该构建的原生界面尚未完成真实账号验收，不应直接作为已验证发布包。

1.7.0-beta.2 发布产物保存在忽略目录 `release/`：安装包约 3.23 MiB，便携版约 13.52 MiB，两者内嵌版本均为 `1.7.0-beta.2`。发布版 CLI 已用临时 home 中的人工账号验证：正常启动，缺登录材料明确返回 `unavailable/login_required`，账号文件字节及文件集合不变。未运行安装器或替换现有应用。

主要覆盖：混入外账号旧凭据仍只请求自身；同命名空间身份冲突在联网前失败；业务身份与 ZCode 身份不同仍合法；人工加密登录对可解密；坏业务 Token 不阻塞自身 Start JWT；A→B→A 使用各自设备 ID；个人/团队项目选择、平台鉴权格式、既有 Key 选择；两个同平台账号实际并发签发、同范围并发合并、项目隔离；更新/删除期间旧签发失效；过期、禁用、401、429 与网络失败；同产品不同项目/有效期及重复窗口；旧 bundle 导入；查询前后文件字节与集合不变；UI 旧响应拒收。

独立后端审查发现的 Start 实例/额度桶关联、`ends_at` 到期过滤、已确认团队信息丢失、未知 Start 状态、坏业务凭据阻塞 Start、团队 Key 列表中坏条目遮挡有效条目，均先用失败用例复现，再修复并通过回归。修复后复审未发现新的阻断问题。

CI 已增加 Node 22 的依赖安装、Node 测试、i18n 与前端构建，保留 Windows/macOS/Linux Rust 检查。此处仅记录本机验证，不预先断言远端 CI 结果。

## 浏览器界面验收

脚本：[`tests/quota-ui.py`](../../tests/quota-ui.py)。Python Playwright + Chromium 执行实际页面与控件，Tauri 边界注入人工账号 A/B/C；禁止所有非本地网络请求。

可在一个终端启动 `npm run dev -- --host 127.0.0.1`，另一个终端运行 `python tests/quota-ui.py`，结束后停止该 Vite 服务。也可使用 webapp-testing 的 `with_server.py`；Windows 上需检查并清理该次服务的子进程，避免占用 Rollup 模块。

已通过：部分失败及初始化提示；刷新失败保留额度和时间；更新凭证后的旧成功丢弃；真实 5 秒状态轮询后的旧失败丢弃；删除时仍在途的响应丢弃；切换活动账号不混用额度；520px 窄窗口无横向溢出。无页面错误、无实际 API 或领取请求。

截图位于忽略目录 `src-tauri/target/quota-ui/`。此项验证浏览器中的实际前端和模拟 IPC，不等同于原生 WebView/真实后端集成验收。

## 11 份账号副本的实际接口检查

检查前已明确：鉴权链路允许为既有 Key 签发短期 Token 的 POST；不创建长期 Key，不领取套餐、不执行激活上报。

使用独立 debug 程序，在唯一的系统临时目录内复制 11 份快照，匿名化 switch 记录 ID/名称，保留认证材料；子进程使用与原加密材料匹配的 secret。临时设置关闭自动领取与启动 ZCode。逐个运行 `--cli --lang en quota --id account-NN`，仅记录匿名编号、完成状态、套餐数量和固定错误码。原始响应、凭据、真实账号/设备 ID 不写入此记录或提交文件。

| 匿名账号 | 返回状态 | 已确认套餐数 | 未完成原因 |
| --- | --- | --- | --- |
| 01、04、05 | complete | 各 0 | 无 |
| 02 | partial | 0 | project_key_missing |
| 03、06、07、08、10、11 | partial | 各 1 | project_key_missing |
| 09 | partial | 0 | network_error |

合计：3 个查询完整完成，7 个缺少可复用项目 Key，1 个遇到网络错误；6 个部分成功账号仍保留已确认的套餐。`partial` 且 0 个套餐不代表已确认未开通。7 个缺 Key 账号没有自动创建 Key，也没有尝试旧密钥取得替代结果。

11 个结果都携带各自匿名账号 ID 和 revision。检查前后对原账号存储、选定 live 文件及副本进行字节摘要和文件集合比较：均未变化。临时凭据目录已删除。脱敏汇总保留在忽略目录 `src-tauri/target/quota-live-summary.json`；一次性运行器同样未纳入提交。

人工传输测试可直接断言每个请求的凭据来源；实际检查没有记录真实请求头，因此不把匿名结果归属检查表述为完整网络流量审计。真实额度的数值准确性仍需官方界面对照。

## 后续验收边界

1. 在隔离的客户端环境中，将代表账号及其余账号与 ZCode 3.16.x 官方界面逐项对照套餐、额度、计量窗口与到期时间。本次未改动当前 ZCode 登录状态，未执行此项。
2. 使用账号副本验证原生 Tauri 窗口与真实后端的刷新、更新、删除及切换。当前已覆盖真实 CLI 后端与浏览器模拟 IPC，二者不能替代这一项。
3. 缺 Key 的账号应先由用户在 ZCode 中完成对应套餐初始化；网络错误账号需在接口可用时复查。未完成前保持明确错误提示。
4. 推送后记录 Windows/macOS/Linux CI 的实际结果；本机 Windows 结果不外推为其他平台通过。

不需要迁移或清理真实账号文件。本次额度验收未安装或替换当前 1.6.0；安装包发布记录以 GitHub Releases 为准。
