//! sidecar 进程生命周期（§4.1 / §10.7 / §12.5 / §12.7）。
//! - Tier 2：以 `node` 外部二进制（随安装包分发）运行已部署的 harness 入口
//!   `dsh-dist/lib/bin.js`（由 pnpm deploy 在打包阶段生成，见 build-windows.yml）。
//! - 就绪探测：stdout URL 行优先，TCP connect 回退。
//! - 进程树回收：优先 Tauri 进程管理；Windows 下 Job Object 兜底（§13.8 D8）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use base64::Engine;
use rand::Rng;
use tauri::{Emitter, Manager};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

use crate::config;
use crate::job;
use crate::proxy;
use crate::state::{AgentStatus, AppState, ChildHandle, LogLine, StateEvent};

/// 启动 dsh sidecar。若已在运行，直接返回当前状态。
pub async fn spawn_dsh(app: &tauri::AppHandle) -> Result<AgentStatus, String> {
    // 防重入
    {
        let inner = app.state::<AppState>().inner.lock().unwrap();
        if inner.child.is_some() {
            return Ok(current_status(app));
        }
    }

    let cfg = app.state::<AppState>().config.lock().unwrap().clone();
    let home = config::resolve_dsh_home(app, &cfg);
    config::write_settings_yaml(&home, &cfg)?;
    let env = config::build_env(&cfg, &home);

    let port = pick_port(cfg.server.port);
    let token = gen_token();

    // Tier 2 sidecar：以 `node` 外部二进制运行已部署的 harness 入口
    // `dsh-dist/lib/bin.js`（pnpm deploy 在打包阶段生成，随安装包分发于资源目录）。
    let resource_dir = app
        .path()
        .resource_dir()
        .map_err(|e| format!("获取资源目录失败: {e}"))?;
    let entry = resource_dir
        .join("dsh-dist")
        .join("lib")
        .join("bin.js");
    if !entry.exists() {
        return Err(format!(
            "找不到 harness 入口: {} (请确认打包阶段已生成 dsh-dist)",
            entry.display()
        ));
    }
    let entry_str = entry.to_string_lossy().to_string();

    let (mut rx, mut child) = app
        .shell()
        .sidecar("node")
        .map_err(|e| e.to_string())?
        .args([entry_str, "web".into(), "--port".into(), port.to_string()])
        .envs(env)
        .spawn()
        .map_err(|e| format!("启动 dsh sidecar 失败: {e}"))?;
    let pid = child.pid();

    // Windows Job Object 兜底（§4.1）
    let job = {
        let j = job::job::JobHandle::new_with_kill_on_close();
        if let Some(ref j) = j {
            let _ = j.assign(pid);
        }
        j
    };

    {
        let mut inner = app.state::<AppState>().inner.lock().unwrap();
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

    let app2 = app.clone();
    let token2 = token.clone();
    tauri::async_runtime::spawn(async move {
        let mut ready = AtomicBool::new(false);
        while let Some(ev) = rx.recv().await {
            match ev {
                CommandEvent::Stdout(b) | CommandEvent::Stderr(b) => {
                    let text = String::from_utf8_lossy(&b).to_string();
                    for raw in text.split('\n') {
                        let line = raw.trim_end().to_string();
                        if line.is_empty() {
                            continue;
                        }
                        let _ = app2.emit(
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
                            let a = app2.clone();
                            let t = token2.clone();
                            on_ready(&a, port, &t).await;
                        }
                    }
                }
                CommandEvent::Terminated(_) => {
                    emit_stopped(&app2, port);
                    break;
                }
                CommandEvent::Error(e) => {
                    let _ = app2.emit(
                        "agent://log",
                        LogLine {
                            stream: "stderr".into(),
                            line: format!("[error] {e}"),
                        },
                    );
                }
            }
        }
    });

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
    match proxy::start_proxy(agent_port, token.to_string()).await {
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
            let _ = app.emit(
                "agent://error",
                format!("本地反代启动失败: {e}"),
            );
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
        let mut inner = app.state::<AppState>().inner.lock().unwrap();
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
