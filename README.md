# DeepSeek Harness Desktop

将 [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) 封装为 Windows 桌面应用（Tauri 2 主方案）。`dsh` 是 Node CLI，经 `dsh web --port <port>` 单进程同时启动 agent 后端并托管 WebUI，监听 `127.0.0.1:PORT`；桌面 Rust 后端以 **Node 22 外部二进制（sidecar）** 运行已部署的 harness 入口 `dsh-dist/lib/bin.js`，harness 依赖以 `pnpm deploy` 物化为 `dsh-dist`（含完整 `node_modules`）随安装包分发。

> **打包方案（当前唯一路径）：Tier 2 侧载。**
> `pkg` / Node SEA 单文件方案（Tier 1）经验证**不可行**：deepseek-harness 是 ESM pnpm 单体仓库，SEA 无法打包其依赖图；V8 code-cache 在 ESM 入口直接报错；原生模块 `node-pty` / `koffi` 在 SEA 下也有问题。因此采用 Tier 2：`node.exe` + `dsh-dist`（完整 `node_modules`）随安装包分发。详见 `desktop/scripts/bundle-dsh.ps1` 顶部注释。

## 技术栈
- **Tauri 2** + **Rust** 桌面外壳，`@tauri-apps/plugin-shell` 以 sidecar 方式启动 `node`
- **WebView2** 渲染前端（React + Vite）
- **Node 22** 作为 sidecar 运行时（内嵌于安装包，用户无需安装 Node）
- 构建 harness 需 **Node 22 + pnpm 11.7.0**，命令 `pnpm install && pnpm run build`

## 目录结构
```
deepseek-harness-desktop/
├─ src/                      # 前端 React 源码
├─ src-tauri/                # Rust 后端 + tauri.conf.json + capabilities/
│  ├─ src/                   # Rust 模块：config / sidecar / proxy / job / tray / state / lib
│  └─ capabilities/          # 最小授权能力配置（sidecar 白名单 + loopback remote 源）
├─ desktop/
│  └─ scripts/              # build-harness.ps1 / bundle-dsh.ps1 / smoke.ps1
├─ deepseek-harness/        # 上游 harness（由 CI 按 UPSTREAM.lock.json 钉 SHA 克隆，不入库）
├─ dsh-dist/                # Tier 2 部署产物（pnpm deploy 物化，构建期生成，不入库）
└─ UPSTREAM.lock.json       # 上游 commit 锁定（CI 据其钉 SHA 克隆，单一事实来源）
```

## 构建 / 打包 / 验收（推荐走 CI）
- 普通提交推送到 `main` 即触发 GitHub Actions（`.github/workflows/build-windows.yml`）：
  钉 SHA 克隆上游 → 构建 harness → `pnpm deploy` 生成 `dsh-dist` → 摆位 `node.exe` sidecar → `tauri build` → **§12.6 硬 Gate 冒烟**（含 node-pty/koffi 原生模块功能探测）→ 上传安装包 Artifact。
- 本地仅验证前端：`npm install && npm run build`（Tauri 完整打包/安装包仍需 Windows + Rust + WebView2 SDK 环境，建议交给 CI）。
- **验收 Gate（§12.6）**：`pwsh desktop/scripts/smoke.ps1 -EntryPath ./dsh-dist/lib/bin.js`，全绿才退出 0。

## 上游同步策略
桌面层**不编辑上游 harness 文件**；上游代码由 CI 按 `UPSTREAM.lock.json` 钉 SHA 克隆（单一事实来源），桌面层只在自己目录 `desktop/`、`src/`、`src-tauri/` 内工作。bump 上游时只需更新 `UPSTREAM.lock.json` 的 `harness.sha` 与 `date`。

## 安全约束（实现要点）
- sidecar 仅能由 Rust 以固定参数 `[<bin.js>, "web", "--port", <port>]` 启动，能力白名单用正则校验（入口须以 `bin.js` 结尾、端口为数字），见 `capabilities/default.json` 与 `src-tauri/src/sidecar.rs`。
- 主窗口已恢复最小 CSP 并声明 loopback 远程源（`http://127.0.0.1:*` / `http://localhost:*`），密钥经 env 注入、WebView 反代带随机会话 token，见 `tauri.conf.json` 与 `src-tauri/src/{config,proxy}.rs`。
- 安装包使用 `webviewInstallMode = downloadBootstrapper`（**不**内嵌 WebView2 离线安装器），
  因此不再多出 ~120 MB；代价是安装时需联网引导 WebView2 运行时。改回内嵌可离线安装，但体积显著增大。

## 运行时加固（孤儿写者锁自愈）
- 背景：上游 `@deepseek-ai/dsh-atomic-write` 用兄弟文件 `<file>.lock`（内容 = 持有者 PID）
  做跨进程写者互斥，默认等 2 秒即失败，且**明确规定孤儿锁的恢复属于运维动作**。
  sidecar 一旦被强杀（taskkill / 应用崩溃 / 宿主进程树被回收），锁就会永久残留，
  此后每次启动都在 2 秒后超时退出 —— 用户侧表现为窗口能开、进程长活，
  但 UI 永久停在「正在启动 Agent…」且无任何报错（2026-09-14 实机 P0）。
- 加固：`src-tauri/src/sidecar.rs` 的 `heal_stale_locks()` 在 `spawn_dsh()` 之前扫描
  `$DSH_HOME` 下的 `*.lock`，仅当**锁内 PID 已退出**且文件年龄 > 5 秒时，将其
  **改名备份**为 `<原名>.orphan.bak_<UTC 时间戳>`（不删除，现场可人工恢复），
  并通过 `agent://log`（`stream = "system"`）上报前端。
- 保守性：活锁（PID 存活 / PID 为 0 / PID 为自身）绝不触碰；内容无法解析为 PID 的锁跳过；
  `node_modules`、`.pnpm`、`target`、`dist` 等目录不递归；受深度（6）与条目数（20000）双重约束。
- 回归闸门：CI `rust-check` job 执行 `cargo test --lib`，覆盖
  「死 PID 隔离 / 活锁保留 / 新锁年龄保护 / 跳过目录与不可解析内容不误伤」四个场景。

## 运行时加固（冷启动自愈引导页）
- 背景（2026-09-14 实机）：**新装 / 升级后的首次启动**时，上游 harness 要先完成初始化
  （生成 `.credentials.yaml` 签名密钥、编译 profile 等），其可服务时刻可能明显晚于代理启动。
  此时 WebView 的首次导航会撞上「会话 cookie 尚未 harvest」，而代理原先直接回一行
  `502 harness auth cookie unavailable` **纯文本** —— 浏览器会把它当成最终页面，
  既不解析 HTML 也**不会自动重试**，UI 就此永久停在加载态（只能人工重启）。
- 加固：`src-tauri/src/proxy.rs` 对「根路径 + `Accept: text/html`」的**页面导航**请求
  改走 `boot_page_response()` —— 返回一个自带轮询的引导页（每 800ms 查 `/__dsh_health`，
  `ready=true` 即 `location.replace` 回原 URL，保留 `?t=` token），
  把「一次性失败」变成「自动等待、就绪即接管」；约 3 分钟仍不可用才降级为「重试」按钮。
  非导航请求（SPA 的 fetch/XHR）行为不变，仍走原有错误码，避免污染接口语义。
- 配套：`/__dsh_health` 新增 `ready` 字段（已 harvest 且未过期），并修正该端点此前输出
  **非法 JSON** 的问题（cookie 值未加引号）；`src/App.tsx` 增加状态兜底轮询 ——
  Tauri 事件不缓存，若 sidecar 在 React 订阅前就绪会丢失 `agent://ready`。
- **循环依赖修复（2026-09-14 实机复现，第二轮）**：`ready` 取决于会话 cookie 是否已 harvest，
  而 harvest **只发生在页面请求路径上**；引导页却只轮询 `/__dsh_health`、不再请求页面 ——
  等于「等一个只有它自己去请求页面才会翻真的标志」，**永远等不到**。实机表现为该端点连续
  180 s 返回 `"cookie":"absent"`、引导页永不跳转。修法两条腿：
  1. `health_handler` 自己驱动一次握手（`ensure_cookie_with(&s, 1)`，单次尝试 → 轮询开销恒定、秒级收敛）；
  2. 引导页在轮询之外，每 4 次（约 3.2 s）用 `HEAD` **真敲一次原地址** —— 这条请求与浏览器导航
     走同一条处理路径（含 token 校验与握手），因此即使该端点的语义将来变化，引导页也一定能收敛，
     不把成败押在单一「就绪标志」上。
  3. 页面导航的等待窗口从 **3 秒放宽到 8 秒**（`wait_cookie_brief`，拿到即返回）。原因：TCP 回退探测
     可能在 stdout 解析出 launch token 之前就触发 `on_ready`，代理启动快照里 token 为空，之后才落盘 ——
     3 秒窗口会在这个空档里超时，于是**每次启动都落到引导页**；8 秒足以覆盖该延迟，
     真正慢的上游仍由引导页兜底。
  - 另：`/__dsh_health` 增加 `agent_token` 布尔字段，用于区分「上游还没起来」与「launch token 没解析到」
    （后者单靠握手重试永远好不了）。
- 回归闸门：`cargo test --lib` 覆盖 `wants_html` 判定 / 引导页回跳目标与轮询特征 /
  **周期性 HEAD 冗余探活** / 内联 JSON 转义 / health 载荷为合法 JSON 且 ready、agent_token 语义正确。

## 已知限制
- 执行 `tauri build` / 出安装包需要 **Windows + Rust (MSVC) + WebView2 SDK** 环境；CI 已封装好，本地一般无需手动出包。
- `dsh web` 就绪行格式依赖实际 harness 输出（`http://127.0.0.1:<port>`），smoke 与其对齐。

## 参考
- 开发文档 §1（上游同步策略）、§4.1（sidecar / Job Object）、§8（端口/威胁模型）、§10.3/§10.7（capability / 反代安全）、§12.3（WebView2 / 远程源）、§12.6（验收 Gate）
