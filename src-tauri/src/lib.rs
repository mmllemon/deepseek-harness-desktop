//! DeepSeek Harness 桌面端 —— Rust 后端入口（Tauri 2）。
//! 模块划分：state（状态/配置结构）、config（配置持久化与密钥）、sidecar（进程生命周期）、
//! proxy（本地反代 token）、job（Windows 进程树回收兜底）、tray（托盘）。

mod config;
mod job;
mod proxy;
mod sidecar;
mod state;
mod tray;

use state::{AgentStatus, AppConfig, AppState};
use tauri::{Emitter, Manager};

/// 启动 dsh sidecar（前端经 Tauri command 调用）。
#[tauri::command]
fn agent_start(app: tauri::AppHandle) -> Result<AgentStatus, String> {
    tauri::async_runtime::block_on(sidecar::spawn_dsh(&app))
}

/// 停止 dsh sidecar。
#[tauri::command]
fn agent_stop(app: tauri::AppHandle) -> Result<(), String> {
    sidecar::stop_dsh(&app)
}

/// 重启 dsh sidecar。
#[tauri::command]
fn agent_restart(app: tauri::AppHandle) -> Result<AgentStatus, String> {
    let _ = sidecar::stop_dsh(&app);
    tauri::async_runtime::block_on(sidecar::spawn_dsh(&app))
}

/// 取当前运行态快照。
#[tauri::command]
fn agent_get_status(app: tauri::AppHandle) -> Result<AgentStatus, String> {
    Ok(sidecar::current_status(&app))
}

/// 读取桌面壳配置（无明文密钥）。
#[tauri::command]
fn config_get(app: tauri::AppHandle) -> Result<AppConfig, String> {
    Ok(app.state::<AppState>().config.lock().unwrap().clone())
}

/// 写入配置：API Key 进 keyring（不落明文），其余持久化 config.json 并翻译写入 settings.yaml。
#[tauri::command]
fn config_set(app: tauri::AppHandle, partial: AppConfig) -> Result<AppConfig, String> {
    config::set_api_key(&partial.model.api_key)?;

    let mut to_save = partial.clone();
    to_save.model.api_key = String::new(); // 不持久化明文密钥
    config::save_config(&app, &to_save)?;

    let home = config::resolve_dsh_home(&app, &to_save);
    config::write_settings_yaml(&home, &to_save)?;

    *app.state::<AppState>().config.lock().unwrap() = to_save.clone();
    Ok(to_save)
}

/// 最小化到托盘。
#[tauri::command]
fn window_minimize_to_tray(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
    Ok(())
}

/// 显示窗口。
#[tauri::command]
fn window_show(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
    Ok(())
}

/// 应用入口（由 src/main.rs 调用）。
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(AppState::default())
        .setup(|app| {
            let handle = app.handle().clone();
            // 载入配置
            let cfg = config::load_config(&handle);
            *handle.state::<AppState>().config.lock().unwrap() = cfg;

            // 托盘
            tray::build_tray(&handle)?;

            // 恢复主题偏好到 localStorage（解决 origin 隔离导致主题丢失的问题）
            // 在窗口创建后注入，因为 setup 时窗口还未存在
            let home = config::resolve_dsh_home(&handle, &cfg);
            if let Some(theme) = config::read_theme_preference(&home) {
                let script = format!(
                    "localStorage.setItem('dsh-angelina-themes.selection', '{}')",
                    theme
                );
                handle.listen_global("tauri://window-created", move |event| {
                    let payload = event.payload();
                    if payload == "main" {
                        if let Some(w) = handle.get_webview_window("main") {
                            let _ = w.eval(&script);
                        }
                    }
                });
            }

            // 自动启动
            let autostart = handle
                .state::<AppState>()
                .config
                .lock()
                .unwrap()
                .server
                .auto_start;
            if autostart {
                let a = handle.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = sidecar::spawn_dsh(&a).await {
                        // 不再静默吞错：回传 UI（查看日志 / 离线面板可见）。
                        let _ = a.emit("agent://error", e);
                    }
                });
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            agent_start,
            agent_stop,
            agent_restart,
            agent_get_status,
            config_get,
            config_set,
            window_minimize_to_tray,
            window_show
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
