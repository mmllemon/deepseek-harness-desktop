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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

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

// ---------------------------------------------------------------------------
// 孤儿写者锁自愈（2026-09-14 实机 P0：应用卡在「正在启动 Agent…」）
//
// 上游 `@deepseek-ai/dsh-atomic-write` 用兄弟文件 `<file>.lock` 做跨进程写者互斥，
// 文件内容只有**持有者 PID**，争用者默认等 2 秒即失败；上游设计明确规定
// 「孤儿锁的恢复是运维动作」（contender 永不删已有锁）。
// 于是 sidecar 一旦被强杀（taskkill / 应用崩溃 / 宿主被回收），锁就永久残留，
// 此后**每一次**启动都在 2 秒后超时退出 —— 用户侧表现：窗口能开、进程长活、
// UI 永久停在「正在启动 Agent…」且没有任何报错。见 PITFALLS.md P24。
// 这里在 spawn 之前主动做一次清理，把「运维动作」自动化掉。
// ---------------------------------------------------------------------------

/// 扫描时**不递归进入**的目录名（大小写不敏感）：这些子树巨大且不可能存放写者锁。
const LOCK_SCAN_SKIP_DIRS: &[&str] = &[
    "node_modules",
    ".pnpm",
    ".git",
    "target",
    "dist",
    "dsh-dist",
    "cache",
    "code cache",
    "gpucache",
];

/// 递归深度上限（`$DSH_HOME` 实际层级很浅，防止异常结构拖慢启动）。
const LOCK_SCAN_MAX_DEPTH: usize = 6;

/// 遍历条目数硬上限，兜底防止极端目录树把启动拖死。
const LOCK_SCAN_MAX_ENTRIES: usize = 20_000;

/// 孤儿锁「最小年龄」：刚创建、内容可能还没写完的锁一律不碰。
const LOCK_MIN_AGE: Duration = Duration::from_secs(5);

/// 判断 PID 对应的进程是否仍存活。
///
/// 用 `PROCESS_QUERY_LIMITED_INFORMATION`（0x1000）而不是 `PROCESS_QUERY_INFORMATION`：
/// 前者对任意完整性级别的进程都能打开，且不像 `PROCESS_ALL_ACCESS` 那样要求提权，
/// 正好用于「存在性探测」。打开失败 = 进程不存在。
#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Threading::OpenProcess;

    // https://learn.microsoft.com/windows/win32/procthread/process-security-and-access-rights
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        // 注意：windows-sys 0.52 的 HANDLE 是 `isize`（不是 `*mut c_void`），
        // 失败返回的是 0 而不是空指针 → 必须用 `== 0` 判断，不能用 `.is_null()`。
        if h == 0 || h == INVALID_HANDLE_VALUE {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// 非 Windows：本应用只发布 Windows 产物，此分支仅为跨平台编译与本地单测服务。
/// `/proc` 存在时按进程目录判断，否则保守返回 `true`（即**永不清理**，宁可不修也不错删）。
#[cfg(not(windows))]
fn process_alive(pid: u32) -> bool {
    if Path::new("/proc").is_dir() {
        PathBuf::from(format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// UTC 时间戳 `YYYYMMDD-HHMMSS`（不引额外依赖，用 Howard Hinnant 的 civil-from-days 算法）。
fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 把「1970-01-01 起的天数」换算为 `(年, 月, 日)`。
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

/// 自愈：清理 `$DSH_HOME` 下由**已死进程**持有的孤儿写者锁（`*.lock`）。
///
/// 保守策略（宁可漏修，绝不误删活锁）：
/// 1. 只处理文件名以 `.lock` 结尾的**普通文件**（跳过目录、符号链接）；
/// 2. 锁内容必须能解析出 PID，否则跳过（可能是别的格式或尚未写完）；
/// 3. PID 为 0、等于自身、或该 PID 仍存活 → 跳过（可能是另一个实例在正常工作）；
/// 4. 文件年龄必须 > `LOCK_MIN_AGE`，避免误伤刚创建的活锁；
/// 5. 判定为孤儿后 **改名备份**为 `<原名>.orphan.bak_<UTC 时间戳>`，**不删除**，
///    现场可人工恢复。
///
/// 返回被隔离的 `(原路径, 已死 PID)` 列表，供上层写日志 / 上报 UI。
fn heal_stale_locks(home: &Path) -> Vec<(PathBuf, u32)> {
    let mut healed: Vec<(PathBuf, u32)> = Vec::new();
    if !home.is_dir() {
        return healed;
    }

    let me = std::process::id();
    let mut budget = LOCK_SCAN_MAX_ENTRIES;
    // 显式栈的深度优先遍历，避免递归爆栈；同时受深度与条目数双重约束。
    let mut stack: Vec<(PathBuf, usize)> = vec![(home.to_path_buf(), 0)];

    while let Some((dir, depth)) = stack.pop() {
        if depth > LOCK_SCAN_MAX_DEPTH || budget == 0 {
            continue;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue, // 目录不可读（权限/瞬时消失）→ 跳过
        };
        for ent in entries.flatten() {
            if budget == 0 {
                break;
            }
            budget -= 1;

            let path = ent.path();
            let ftype = match ent.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };

            if ftype.is_dir() {
                let name = ent.file_name().to_string_lossy().to_ascii_lowercase();
                if LOCK_SCAN_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((path, depth + 1));
                continue;
            }
            if !ftype.is_file() {
                continue; // 符号链接 / 其它特殊文件一律不动
            }

            let fname = ent.file_name().to_string_lossy().into_owned();
            if !fname.to_ascii_lowercase().ends_with(".lock") {
                continue;
            }

            // 上游约定：锁内容 = 持有者 PID（一行数字）。
            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let pid = match content
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<u32>().ok())
            {
                Some(p) => p,
                None => continue,
            };
            if pid == 0 || pid == me || process_alive(pid) {
                continue;
            }

            // 年龄保护：刚创建的文件可能正处在「已建锁、未写入 PID」的正常窗口内。
            // 取不到 mtime 时按「太新」处理（宁可漏修，不可误删）。
            let too_fresh = ent
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| SystemTime::now().duration_since(t).unwrap_or_default() < LOCK_MIN_AGE)
                .unwrap_or(true);
            if too_fresh {
                continue;
            }

            let backup = path.with_file_name(format!("{fname}.orphan.bak_{}", utc_stamp()));
            if std::fs::rename(&path, &backup).is_ok() {
                healed.push((path, pid));
            }
        }
    }

    healed
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

    // 自愈：先隔离上次强杀/崩溃遗留的孤儿写者锁，否则 sidecar 必在 2 秒后超时退出，
    // UI 永久卡在「正在启动 Agent…」且无任何报错（2026-09-14 实机 P0，见 heal_stale_locks）。
    let healed_locks = heal_stale_locks(&home);
    if !healed_locks.is_empty() {
        for (path, pid) in &healed_locks {
            let line = format!(
                "[self-heal] 已隔离孤儿写者锁（持有者 PID {pid} 已退出，原文件已改名备份）: {}",
                path.display()
            );
            let _ = app.emit(
                "agent://log",
                LogLine {
                    stream: "system".into(),
                    line,
                },
            );
        }
    }

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
        .arg("--no-open")
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
                    // 提取上游 harness launch token（alpha.3+ 新增鉴权所需，用于换取签名 cookie）
                    let agent_tok = extract_agent_token(&line);
                    if let Some(ref at) = agent_tok {
                        let st = app_out.state::<AppState>();
                        st.inner.lock().unwrap().agent_token = Some(at.clone());
                    }
                    let a = app_out.clone();
                    let t = tok_out.clone();
                    tauri::async_runtime::spawn(async move {
                        on_ready(&a, port, &t, agent_tok).await;
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
                let (already, agent_tok) = {
                    let st = app3.state::<AppState>();
                    let inner = st.inner.lock().unwrap();
                    (inner.proxy_url.is_some(), inner.agent_token.clone())
                };
                if !already {
                    on_ready(&app3, port, &token3, agent_tok).await;
                }
                break;
            }
        }
    });

    Ok(current_status(app))
}

/// 反代启动成功后的统一回调：拉起 axum 反代 + 通知前端加载 proxyUrl。
async fn on_ready(app: &tauri::AppHandle, agent_port: u16, token: &str, agent_token: Option<String>) {
    // 幂等：仅首次生效
    {
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();
        if inner.state == "running" {
            return;
        }
    }
    // 修复（2026-09-01）：优先用传入 token，为空则取 AppState「实时」agent_token，
    // 避免 TCP 回退路径在 stdout 解析出 token 之前就触发 on_ready 而传入空快照
    // （那会导致代理以空 token 启动、cookie 永远 harvest 不到 → 502）。
    let live = {
        // 必须先把 app.state::<AppState>() 绑定到 let，否则临时 State 值在语句末尾释放，
        // 而 .inner.lock() 返回的 MutexGuard 仍借用它，触发 E0716（同文件 230-235 行的写法）。
        let state = app.state::<AppState>();
        let inner = state.inner.lock().unwrap();
        inner.agent_token.clone()
    };
    let agent_token = agent_token.or(live);
    match proxy::start_proxy(
        agent_port,
        token.to_string(),
        app.clone(),
        {
            let cfg = app.state::<AppState>().config.lock().unwrap().clone();
            if cfg.ui.theme.is_empty() { None } else { Some(cfg.ui.theme) }
        },
        agent_token,
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

#[allow(dead_code)] // 保留：代理意外退出时刷新前端 stopped 状态的事件入口
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

/// 显式解析 node 外部二进制：优先 triple 命名（构建脚本产出），回退到 node.exe。
/// 路径搜索顺序：resource_dir > exe_dir（与 tauri externalBin 约定对齐）。
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
    // 构建脚本将 node.exe 复制到 src-tauri/ 和 src-tauri/binaries/ 两个位置（见 build-windows.yml），
    // Tauri 的 externalBin 机制会把它们放到 resource_dir/（triple 名）或 resource_dir/binaries/。
    // 此处按优先级查找：triple 名优先（精确匹配），其次通用名。
    let candidates = [
        resource_dir.join("node-x86_64-pc-windows-msvc.exe"),
        exe_dir.join("node-x86_64-pc-windows-msvc.exe"),
        resource_dir.join("node.exe"),
        exe_dir.join("node.exe"),
        resource_dir.join("binaries").join("node-x86_64-pc-windows-msvc.exe"),
        exe_dir.join("binaries").join("node-x86_64-pc-windows-msvc.exe"),
        resource_dir.join("binaries").join("node.exe"),
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

/// 从 `dsh web:` 启动行提取 harness launch token（alpha.3+ 鉴权所需）。
/// 形如 `dsh web: http://127.0.0.1:PORT/?token=<43-char>&...` 或带 `(LAN: ...)` 后缀。
/// 返回 `?token=` 之后的纯 token 串（不含后续 `&` / 空白 / 括号）。
fn extract_agent_token(line: &str) -> Option<String> {
    let idx = line.find("token=")?;
    let rest = &line[idx + "token=".len()..];
    let end = rest
        .find(|c: char| ['&', ' ', '\t', '\r', '\n', ')', '"', '\''].contains(&c))
        .unwrap_or(rest.len());
    let tok = &rest[..end];
    if tok.is_empty() { None } else { Some(tok.to_string()) }
}

// ---------------------------------------------------------------------------
// 单元测试：孤儿写者锁自愈（CI `cargo test --lib` 闸门）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 一个**保证不可能存在**的 PID：Windows PID 为 4 的倍数且远小于该值，
    /// Linux 也不可能分配到 `/proc/4294967294`。
    const DEAD_PID: u32 = u32::MAX - 1;

    /// 建一个隔离的临时 `$DSH_HOME`（含 `profiles/` 子目录）。
    fn temp_home(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("dsh-heal-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(p.join("profiles")).unwrap();
        p
    }

    /// 写一个内容为 PID 的锁文件，并把 mtime 往前拨 1 小时，
    /// 以绕过 `LOCK_MIN_AGE` 的「新文件不碰」保护。
    fn write_aged_lock(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
        if let Ok(f) = fs::OpenOptions::new().write(true).open(path) {
            let old = SystemTime::now() - Duration::from_secs(3600);
            let _ = f.set_modified(old);
        }
    }

    fn backup_count(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".orphan.bak_"))
            .count()
    }

    /// 已死 PID 的锁必须被隔离：原路径消失 + 留下改名备份（不删除）。
    #[test]
    fn heals_lock_held_by_dead_pid() {
        let home = temp_home("dead");
        let lock = home.join("profiles").join("node_modules.lock");
        write_aged_lock(&lock, &DEAD_PID.to_string());

        let healed = heal_stale_locks(&home);

        assert_eq!(healed.len(), 1, "应恰好隔离 1 个孤儿锁，实际 {healed:?}");
        assert_eq!(healed[0].1, DEAD_PID);
        assert!(!lock.exists(), "原锁文件应已改名");
        assert_eq!(
            backup_count(&home.join("profiles")),
            1,
            "应留下 1 个改名备份"
        );

        let _ = fs::remove_dir_all(&home);
    }

    /// 活锁（持有者 = 本进程）绝不能被碰。
    #[test]
    fn keeps_lock_held_by_live_process() {
        let home = temp_home("live");
        let lock = home.join("profiles").join("node_modules.lock");
        write_aged_lock(&lock, &std::process::id().to_string());

        let healed = heal_stale_locks(&home);

        assert!(healed.is_empty(), "活锁不应被清理: {healed:?}");
        assert!(lock.exists(), "活锁文件必须原样保留");
        assert_eq!(backup_count(&home.join("profiles")), 0);

        let _ = fs::remove_dir_all(&home);
    }

    /// 刚创建的锁（未过年龄阈值）应被放过，即使 PID 已死。
    #[test]
    fn keeps_fresh_lock_even_with_dead_pid() {
        let home = temp_home("fresh");
        let lock = home.join("profiles").join("node_modules.lock");
        fs::write(&lock, DEAD_PID.to_string()).unwrap(); // 不拨 mtime → 刚刚创建

        let healed = heal_stale_locks(&home);

        assert!(healed.is_empty(), "新锁应被年龄保护放过: {healed:?}");
        assert!(lock.exists());

        let _ = fs::remove_dir_all(&home);
    }

    /// 非锁文件、内容无法解析为 PID 的锁、以及跳过目录内的锁，一律不动。
    #[test]
    fn ignores_non_lock_unparsable_and_skipped_dirs() {
        let home = temp_home("ignore");

        let notes = home.join("profiles").join("notes.txt");
        fs::write(&notes, DEAD_PID.to_string()).unwrap();

        let weird = home.join("profiles").join("weird.lock");
        write_aged_lock(&weird, "not-a-pid\n");

        // node_modules 在跳过名单内 → 其中的锁不应被扫描到
        let nested_dir = home.join("profiles").join("node_modules");
        fs::create_dir_all(&nested_dir).unwrap();
        let nested = nested_dir.join("pkg.lock");
        write_aged_lock(&nested, &DEAD_PID.to_string());

        let healed = heal_stale_locks(&home);

        assert!(healed.is_empty(), "不应清理任何文件: {healed:?}");
        assert!(notes.exists() && weird.exists() && nested.exists());

        let _ = fs::remove_dir_all(&home);
    }

    /// `utc_stamp()` 输出形如 `YYYYMMDD-HHMMSS`（用于备份文件名可读性）。
    #[test]
    fn utc_stamp_shape() {
        let s = utc_stamp();
        assert_eq!(s.len(), 15, "时间戳长度应为 15: {s}");
        assert_eq!(&s[8..9], "-");
        assert!(s[..8].chars().all(|c| c.is_ascii_digit()));
    }
}
