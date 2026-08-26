//! sidecar 进程生命周期（§4.1 / §10.7 / §12.5 / §12.7）。
//! - Tier 2：以 `node` 外部二进制（随安装包分发）运行已部署的 harness 入口
//!   `dsh-dist/lib/bin.js`（由 pnpm deploy 在打包阶段生成，见 build-windows.yml）。
//! - 关键修复（2026-08）：不再依赖 `tauri_plugin_shell::sidecar()`。
//!   该 API 依赖 Tauri 在构建期嵌入的外部二进制清单 + `shell:allow-execute` 作用域，
//!   在 NSIS 扁平安装（资源直接落在安装根目录而非 `resources/` 子目录）时，
//!   `sidecar("node")` 的内部解析/权限校验会静默失败且错误被吞掉，导致 Agent 永远卡在
//!   「正在启动 Agent…」。改为直接用 `std::process::Command` 以显式绝对路径启动 node，
//!   只要求安装目录下存在 node 二进制即可，与 Tauri 的 sidecar 解析机制彻底解耦。
//! - 就绪探测：stdout URL 行优先，TCP connect 回退。
//! - 进程树回收：优先 Tauri 进程管理；Windows 下 Job Object 兜底（§13.8 D8）。

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use tauri::{Emitter, Manager};

use crate::config;
use crate::job;
use crate::proxy;
use crate::state::{AgentStatus, AppState, ChildHandle, LogLine, StateEvent};

/// 去掉 Windows verbatim 路径前缀（`\\?\` / `\\?\UNC\`）。
/// `std::env::current_exe()` 与 `fs::canonicalize()` 在 Windows 会返回该前缀，
/// 直接作为 node 的脚本参数会导致 `Cannot find module 'D:\?\D:\...'` 而立即退出。
#[cfg(windows)]
fn normalize_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy().into_owned();
    let stripped: &str = if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        rest
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest
    } else {
        &s
    };
    PathBuf::from(stripped)
}
#[cfg(not(windows))]
fn normalize_path(p: PathBuf) -> PathBuf {
    p
}

/// 启动 dsh sidecar。若已在运行，直接返回当前状态。
pub async fn spawn_dsh(app: &tauri::AppHandle) -> Result<AgentStatus, String> {
    // 防重入：先释放锁再取状态，避免 current_status 内部再次加锁造成死锁。
    let already_running = {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();
        inner.child.is_some()
    };
    if already_running {
        return Ok(current_status(app));
    }

    let cfg = app.state::<AppState>().config.lock().unwrap().clone();
    let home = config::resolve_dsh_home(app, &cfg);
    config::write_settings_yaml(&home, &cfg)?;
    let env = config::build_env(&cfg, &home);

    let port = pick_port(cfg.server.port);
    let token = gen_token();

    // 直接启动 node 外部二进制（显式解析路径，绕开 Tauri sidecar 机制）。
    // Windows 下 current_exe()/resource_dir() 可能返回 `\\?\` verbatim 前缀，
    // node 无法把带该前缀的路径当作脚本模块解析（Cannot find module），
    // 必须先 normalize 去掉前缀再传给 node（2026-08 修复）。
    let node_bin = normalize_path(resolve_node(app)?);
    let entry = normalize_path(resolve_entry(app)?);

    let mut cmd = Command::new(&node_bin);
    cmd.arg(&entry)
        .arg("web")
        .arg("--port")
        .arg(port.to_string())
        .envs(&env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Windows 下隐藏控制台窗口（CREATE_NO_WINDOW = 0x08000000）
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!(
                "启动 dsh sidecar 失败: {e} (node={}, entry={})",
                node_bin.display(),
                entry.display()
            );
            // 回传 UI（查看日志 / 离线面板可见），避免静默卡死。
            let _ = app.emit("agent://error", msg.clone());
            return Err(msg);
        }
    };
    let pid = child.id();

    // 取走 stdout/stderr 管道，交给读取线程（child 本体随后存入状态）。
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Windows Job Object 兜底（§4.1）
    let job = {
        let j = job::job::JobHandle::new_with_kill_on_close();
        if let Some(ref j) = j {
            let _ = j.assign(pid);
        }
        j
    };

    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        inner.child = Some(ChildHandle {
            pid,
            child,
            #[cfg(windows)]
            job,
        });
        inner.token = Some(token.clone());
        inner.agent_port = Some(port);
        inner.state = "starting".into();
        inner.last_error = None;
    }

    // stdout 读取线程：转发日志 + 就绪探测（URL 行）。
    if let Some(out) = stdout {
        let app_out = app.clone();
        let tok_out = token.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(out);
            let ready = AtomicBool::new(false);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = app_out.emit(
                    "agent://log",
                    LogLine {
                        stream: "stdout".into(),
                        line: line.clone(),
                    },
                );
                // 就绪探测：stdout URL 行（§12.7）
                if !ready.load(Ordering::SeqCst)
                    && line.contains("http://127.0.0.1")
                    && line.contains(&format!(":{port}"))
                {
                    ready.store(true, Ordering::SeqCst);
                    let a = app_out.clone();
                    let t = tok_out.clone();
                    tauri::async_runtime::spawn(async move {
                        on_ready(&a, port, &t).await;
                    });
                }
            }
        });
    }

    // stderr 读取线程：仅转发日志。
    if let Some(err) = stderr {
        let app_err = app.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(err);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    continue;
                }
                let _ = app_err.emit(
                    "agent://log",
                    LogLine {
                        stream: "stderr".into(),
                        line,
                    },
                );
            }
        });
    }

    // TCP 回退探测（§12.7）
    let app3 = app.clone();
    let token3 = token.clone();
    tauri::async_runtime::spawn(async move {
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if tcp_connect("127.0.0.1", port) {
                let already = {
                    app3.state::<AppState>()
                        .inner
                        .lock()
                        .unwrap()
                        .proxy_url
                        .is_some()
                };
                if !already {
                    on_ready(&app3, port, &token3).await;
                }
                break;
            }
        }
    });

    Ok(current_status(app))
}

/// 反代启动成功后的统一回调：拉起 axum 反代 + 通知前端加载 proxyUrl。
async fn on_ready(app: &tauri::AppHandle, agent_port: u16, token: &str) {
    // 幂等：仅首次生效
    {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();
        if inner.state == "running" {
            return;
        }
    }
    match proxy::start_proxy(
        agent_port,
        token.to_string(),
        app.clone(),
        {
            let cfg = app.state::<AppState>().config.lock().unwrap().clone();
            if cfg.ui.theme.is_empty() { None } else { Some(cfg.ui.theme) }
        },
    ).await {
        Ok((proxy_port, proxy_url)) => {
            let state = app.state::<AppState>();
            let pid = state
                .inner
                .lock()
                .unwrap()
                .child
                .as_ref()
                .map(|c| c.pid);
            {
                let mut inner = state.inner.lock().unwrap();
                inner.proxy_port = Some(proxy_port);
                inner.proxy_url = Some(proxy_url.clone());
                inner.agent_port = Some(agent_port);
                inner.state = "running".into();
            }
            let _ = app.emit(
                "agent://ready",
                serde_json::json!({
                    "proxyUrl": proxy_url,
                    "agentPort": agent_port,
                    "proxyPort": proxy_port
                }),
            );
            let _ = app.emit(
                "agent://state",
                StateEvent {
                    state: "running".into(),
                    proxy_url,
                    agent_port,
                    proxy_port,
                    pid,
                },
            );
        }
        Err(e) => {
            let _ = app.emit("agent://error", format!("本地反代启动失败: {e}"));
        }
    }
}

fn emit_stopped(app: &tauri::AppHandle, agent_port: u16) {
    let _ = app.emit(
        "agent://state",
        StateEvent {
            state: "stopped".into(),
            proxy_url: String::new(),
            agent_port,
            proxy_port: 0,
            pid: None,
        },
    );
    let state = app.state::<AppState>();
    let mut inner = state.inner.lock().unwrap();
    inner.state = "stopped".into();
    inner.child = None;
}

/// 停止 sidecar：先优雅 kill，再以 Job Object 兜底回收进程树（§4.1）。
pub fn stop_dsh(app: &tauri::AppHandle) -> Result<(), String> {
    let child_opt = app.state::<AppState>().inner.lock().unwrap().child.take();
    if let Some(mut handle) = child_opt {
        let _ = handle.child.kill();
        #[cfg(windows)]
        if let Some(job) = handle.job.take() {
            job.terminate();
        }
    }
    {
        let state = app.state::<AppState>();
        let mut inner = state.inner.lock().unwrap();
        inner.state = "stopped".into();
        inner.token = None;
        inner.proxy_port = None;
        inner.proxy_url = None;
        inner.agent_port = None;
        inner.child = None;
    }
    Ok(())
}

pub fn current_status(app: &tauri::AppHandle) -> AgentStatus {
    let state = app.state::<AppState>();
    let inner = state.inner.lock().unwrap();
    AgentStatus {
        state: inner.state.clone(),
        agent_port: inner.agent_port.unwrap_or(0),
        proxy_port: inner.proxy_port.unwrap_or(0),
        proxy_url: inner.proxy_url.clone().unwrap_or_default(),
        pid: inner.child.as_ref().map(|c| c.pid),
    }
}

/// 显式解析 node 外部二进制：优先资源目录/安装根目录下的 triple 命名文件，
/// 回退到 `node.exe`，并兼容 `binaries/` 子目录。任一存在即采用。
fn resolve_node(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("获取 exe 路径失败: {e}"))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "无法获取 exe 父目录".to_string())?;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("获取资源目录失败: {e}"))?;
    let candidates = [
        resource_dir.join("node-x86_64-pc-windows-msvc.exe"),
        resource_dir.join("node.exe"),
        exe_dir.join("node-x86_64-pc-windows-msvc.exe"),
        exe_dir.join("node.exe"),
        resource_dir.join("binaries").join("node-x86_64-pc-windows-msvc.exe"),
        resource_dir.join("binaries").join("node.exe"),
        exe_dir.join("binaries").join("node-x86_64-pc-windows-msvc.exe"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "找不到 node 外部二进制，已尝试以下路径:\n{}",
        candidates
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

/// 显式解析 harness 入口：资源目录或安装根目录下的 `dsh-dist/lib/bin.js`。
fn resolve_entry(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("获取 exe 路径失败: {e}"))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "无法获取 exe 父目录".to_string())?;
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("获取资源目录失败: {e}"))?;
    let bases = [resource_dir, exe_dir];
    for base in &bases {
        let e = base.join("dsh-dist").join("lib").join("bin.js");
        if e.exists() {
            return Ok(e);
        }
    }
    Err(format!(
        "找不到 harness 入口 dsh-dist/lib/bin.js，已尝试: {:?}",
        bases
            .iter()
            .map(|b| b.join("dsh-dist").join("lib").join("bin.js"))
            .collect::<Vec<_>>()
    ))
}

/// 端口选择：优先首选端口，被占则顺延（§8 端口占用）。
fn pick_port(preferred: u16) -> u16 {
    for p in preferred..=preferred + 100 {
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            return p;
        }
    }
    preferred
}

/// 生成随机会话 token（32 字节 URL-safe base64）。
fn gen_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill(&mut b);
    base64::engine::general_purpose::URL_SAFE.encode(b)
}

fn tcp_connect(host: &str, port: u16) -> bool {
    match format!("{host}:{port}").parse::<std::net::SocketAddr>() {
        Ok(addr) => std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok(),
        Err(_) => false,
    }
}
