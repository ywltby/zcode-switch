# Z·SWITCH (zcode-switch)

**简体中文** ｜ [English](README.en.md)

Tauri 2 桌面工具：在多个 ZCode 账号之间一键切换，自动显示额度。只换登录身份——项目、会话、设置全部共用不动。

![screenshot](docs/screenshot.png)

## 功能

- **保存 / 切换账号**：一键切换登录身份；切换前自动保全当前登录，已过期的登录会阻止切换；保留 ZCode 的本机设备和监控状态，账号各自保留查询与领取所需的设备标识
- **添加账号**：工具内 OAuth 登录新号（BigModel / z.ai 双入口），全程不动当前登录
- **额度展示**：账号行内联显示套餐额度与重置时间，多套餐分组；Start Plan 按每项权益的实际生效时间显示，未来套餐标记待生效，过期套餐隐藏
- **活动领取**：可领套餐来源同时覆盖结算预览与官方客户端的营销活动下发，「当前登录」未保存也能直接查与领；账号行「查资格」只刷新该账号，顶部「刷新资格」扫描全部；都会尝试活动激活上报，查到可领后立即加入后台串行领取队列，扫描继续。「自动领取」开关（默认关）只控制每 10 分钟定时扫描，关闭不取消已排队任务
- **并发刷新**：额度与领取资格共享最多 3 个应用层任务槽位，同一账号同一版本只查一次；批量资格刷新不再逐账号等待 5 秒
- **领取恢复**：手动、批量、自动共用一个后端领取会话；提交中、结果待确认与明确失败分开显示，迟到的结果仍会回填；重启不会自动重放未确认的提交
- **加密导入导出**：`.zsb` 捆绑包，PBKDF2(100k) + AES-256-GCM 口令加密；新备份保留账号的查询与领取设备标识，旧备份仍可导入
- **中英双语**：设置里一键切换 中文 / English，主窗、托盘、错误提示、CLI 输出全覆盖；首次运行按系统语言自动选择
- **托盘 / 开机自启 / CLI 自动化**

## 安全设计

- **本地优先**：所有数据在本地，无遥测、无远端存储；额度查询直连官方接口
- **WebView CSP**：`script-src` 基线为 `'self'`；为官方活动的网页组件放行了最小范围的第三方脚本与图片来源，界面事件不依赖动态执行、走白名单式分发
- **防丢号**：切换前自动保全未入库登录；文件写入走临时文件 + 原子 rename
- **路径穿越防护**：账号 id 白名单（`[A-Za-z0-9-]`），删除/读取均不可逃出账号库目录
- **加密导出**：PBKDF2-HMAC-SHA256（100k 迭代）+ AES-256-GCM，随机 salt/nonce；错密码即失败，无明文痕迹
- **凭据在本地解密**：用于显示账号资料及该账号的官方接口鉴权；导出文件凭口令加密

## CLI

### 额度查询兼容性

额度查询按 ZCode 3.16.x 的登录与项目 Token 协议实现。已有账号快照和旧 `.zsb` 可继续读取，无需批量迁移；旧 provider 密钥不再用于额度查询。缺少登录、设备或项目初始化材料时，界面会给出对应提示。3.11–3.14 的历史查询协议不保证完整兼容。

每次查询固定账号、登录代次和项目范围，使用该账号已有设备 ID。Coding Plan 只复用已存在的 API Key ID，并通过 **POST 签发短期项目 Token**；Token 仅驻留内存。查询不创建组织、项目或长期 API Key，不领取套餐，也不写账号、live credentials、配置或设备文件。不同团队项目分别展示；部分渠道失败会保留已确认的套餐并提示失败。凭证更新或账号删除后，旧请求不能回填。

本地测试与实际接口验收进度见 [额度隔离验收记录](docs/testing/quota-account-isolation.md)。离线测试通过不代表全部真实账号已通过验收。

并发刷新、领取会话与验证码恢复的测试边界见 [并发刷新与领取恢复验收记录](docs/testing/concurrent-refresh-claim-recovery.md)。该记录区分离线、浏览器、Windows 原生与线上副作用验收；原生与线上部分仍需在有测试账号的机器上完成。

```
zcode-switch.exe --cli state|list
zcode-switch.exe --cli quota [--id <账号id>]
zcode-switch.exe --cli claim-preview [--id <账号id>|live]
zcode-switch.exe --cli capture [--name 名称]
zcode-switch.exe --cli switch --id <id> [--force] [--restart|--no-restart] [--hot <bool>|--no-hot]
zcode-switch.exe --cli kill
zcode-switch.exe --cli export --id <id> --out <a.zsb>
zcode-switch.exe --cli export-all --out <all.zsb>
zcode-switch.exe --cli import --file <file.zsb>
zcode-switch.exe --cli rename|delete|update|behavior|setpath|launch
zcode-switch.exe --cli --lang en state              # 英文输出（--lang 空格传值、可置于任意位置；默认跟 GUI 语言/系统语言）
```

CLI 密码（export / import）：优先环境变量 `ZSW_PASSWORD`（不出现在进程列表和命令历史），也可 `--password <密码>`。

## 常见问题

### macOS 弹出「麦克风 / 辅助功能 / 录屏」权限询问？

全部拒绝即可，不影响任何功能。
应用内嵌的登录等网页由系统 WebView 渲染，网页发起的请求会被透传为应用的系统权限询问；应用本体与其内嵌网页均未使用这三项能力。

## 构建

```bash
npm install
npm run tauri dev      # 开发（HMR）
npm run tauri build    # NSIS 安装包
```

Windows 优先（路径探测 / 进程管理 / 托盘均为 Win32 语义）。

## License

[MIT](./LICENSE)
