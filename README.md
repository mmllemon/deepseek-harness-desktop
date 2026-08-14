# DeepSeek Harness Desktop

将 [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness) 封装为 Windows 桌面应用（Tauri 2 主方案）。`dsh` 是 Node CLI，经 `dsh web --port <port>` 单进程同时启动 agent 后端并托管 WebUI，监听 `127.0.0.1:PORT`；桌面 Rust 后端以 Node 22 SEA 产出的单文件 `dsh.exe` 作为 Tauri externalBin sidecar 拉起。

## 技术栈
- **Tauri 2** + **Rust** 桌面外壳，`@tauri-apps/plugin-shell` 以 sidecar 方式启动 `dsh`
- **WebView2** 渲染前端（React + Vite）
- **Node 22 SEA** 将 harness 打包为单文件 `dsh-x86_64-pc-windows-msvc.exe`（内嵌 Node，无需用户安装 Node）
- 构建 harness 需 **Node 22 + pnpm 11.7.0**，命令 `pnpm install && pnpm run build`（产出 `apps/cli/lib` 与 `apps/web/dist`）

## 目录结构
```
deepseek-harness-desktop/
├─ src/                      # 前端 React 源码
├─ src-tauri/                # Rust 后端 + tauri.conf.json + externalBin/
│  └─ externalBin/
│     └─ dsh-x86_64-pc-windows-msvc.exe   # 由 SEA 产出的 sidecar（不入库，按 UPSTREAM.lock.json 重建）
├─ scripts/
│  ├─ seapack.cjs           # Node 22 SEA 打包逻辑（Tier 1）
│  └─ sea-config.json       # SEA 配置（main 指向构建后的 harness 入口）
├─ desktop/
│  ├─ scripts/              # build-harness.ps1 / setup.ps1 / release.ps1 / smoke.ps1
│  ├─ config/               # 桌面运行时配置 schema 与默认值
│  └─ assets/
├─ deepseek-harness/        # 上游 harness（fork 克隆 / 子模块 / 子目录，见同步策略）
├─ dist/dsh.exe             # SEA 本地产物
└─ UPSTREAM.lock.json       # 上游 commit / dsh 二进制 sha 锁定
```

## 开发 / 构建 / 打包命令
- **初始化（首次）**：`pwsh desktop/scripts/setup.ps1 -ForkUrl <your-fork> [-UpstreamUrl <upstream>]`
- **仅重建 harness**：`pwsh desktop/scripts/build-harness.ps1 [-HarnessDir ./deepseek-harness]`
- **本地出包**：`pwsh desktop/scripts/release.ps1` —— 构建 harness → SEA 打包 → 落盘 externalBin → `npm ci` → `npm run build` → `npm run tauri build`
- **验收 Gate（§12.6）**：`pwsh desktop/scripts/smoke.ps1 [-DshPath ./dist/dsh.exe]`，全绿才退出 0
- **日常前端开发**：`npm run dev`（Tauri dev 由 `npm run tauri dev` 触发）

## 上游同步策略
桌面层**不编辑上游 harness 文件**；上游代码通过以下方式之一引入（引用开发文档 §1，桌面层只在自己目录 `desktop/`、`scripts/`、`src/`、`src-tauri/` 内工作）：
- **A. Git 子模块**（推荐，CI 用 `submodules: recursive`）
- **B. Fork + upstream**：`git clone <fork>` 后 `git remote add upstream <upstream>`，定期 `git fetch upstream && git merge upstream/master`
- **C. subtree**

同步后用 `UPSTREAM.lock.json` 记录对齐的 commit sha 与 `dsh` 二进制 sha。

## 打包兼容性矩阵（引用 §12.6）
| 方案 | 状态 | 说明 |
|------|------|------|
| **Node 22 SEA 单文件** | ✅ 主路径（Tier 1） | `scripts/seapack.cjs` 产出内嵌 Node 的 `dsh.exe`，最优分发体验 |
| **侧载 `node.exe` + `node_modules`** | 🟡 兜底（Tier 2） | 当 node-pty / koffi 等原生模块在 SEA 下验证不通过时改用；随安装包分发 Node 运行时 |
| **@yao-pkg/pkg** | 🟡 兼容 | 社区维护的 pkg 分支，可作备选打包器 |
| **官方 `pkg` (vercel)`** | ❌ 淘汰 | 已停止维护，不与 Node 22 / 新原生模块兼容 |

> SEA 属 **Active Development**，须将 Node 版本钉死在 22.x（见 `sea-config.json` 与 CI `setup-node@v4`）。若 Tier 1 失败，按 seapack.cjs 顶部提示切换 Tier 2。

## 已知限制
- 执行 `tauri build` 需要 **Windows + Rust (MSVC) + WebView2 SDK** 环境；纯前端 / CLI 验证可在其他平台进行。
- **SEA 属 Active Development**：Node 版本须严格钉在 22.x，升级 Node 大版本需回归 seapack 与 smoke 验证。
- `src-tauri/externalBin/dsh-x86_64-pc-windows-msvc.exe` **不入库**，按 `UPSTREAM.lock.json` 记录的 sha 通过 `release.ps1` 重建；CI 中由构建产物缓存复用。
- `dsh web` 就绪行格式依赖实际 harness 输出（`http://127.0.0.1:<port>`），smoke 与其对齐；API 可达性探测（§12.6 d）仅在设置了 `DEEPSEEK_API_KEY` 时执行，且不计失败。

## 参考
- 开发文档 §1（上游同步策略）、§6.2、§12.2、§12.4（CI）、§12.6（兼容性矩阵 / 验收 Gate）
