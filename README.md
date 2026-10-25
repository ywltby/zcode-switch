# Z·SWITCH (zcode-switch)

**简体中文** ｜ [English](README.en.md)

Tauri 2 桌面工具：在多个 ZCode 账号之间一键切换，自动显示额度。只换登录身份——项目、会话、设置全部共用不动。

![screenshot](docs/screenshot.png)

## 功能

- **保存 / 切换账号**：一键切换登录身份；切换前自动保全当前登录，绝不丢号；设备身份跟账号走，远程控制中继密钥跨切换保活
- **添加账号**：工具内 OAuth 登录新号（BigModel / z.ai 双入口），全程不动当前登录
- **额度展示**：账号行内联显示套餐额度与重置时间，多套餐分组
- **活动领取**：可领套餐一键领取；「自动领取」开关（默认关）定时自动检测并领取，手动操作优先
- **加密导入导出**：`.zsb` 捆绑包，PBKDF2(100k) + AES-256-GCM 口令加密
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

```
zcode-switch.exe --cli state|list
zcode-switch.exe --cli quota [--id <账号id>]
zcode-switch.exe --cli claim-preview [--id <账号id>]
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
