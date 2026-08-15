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
- 安装包内嵌 WebView2 离线安装器（`offlineInstaller`），故**安装包约 330 MB**；如需更小体积可改 `downloadBootstrapper`（代价：安装时需联网）。

## 已知限制
- 执行 `tauri build` / 出安装包需要 **Windows + Rust (MSVC) + WebView2 SDK** 环境；CI 已封装好，本地一般无需手动出包。
- `dsh web` 就绪行格式依赖实际 harness 输出（`http://127.0.0.1:<port>`），smoke 与其对齐。

## 参考
- 开发文档 §1（上游同步策略）、§4.1（sidecar / Job Object）、§8（端口/威胁模型）、§10.3/§10.7（capability / 反代安全）、§12.3（WebView2 / 远程源）、§12.6（验收 Gate）
