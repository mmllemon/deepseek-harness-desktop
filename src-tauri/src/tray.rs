//! 系统托盘与菜单（§4.4）。
//! 左键打开窗口；右键菜单：启动 / 停止 / 重启 / 设置 / 退出。

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::Manager;

pub fn build_tray(app: &tauri::AppHandle) -> tauri::Result<()> {
    let start = MenuItem::with_id(app, "start", "启动 Agent", true)?;
    let stop = MenuItem::with_id(app, "stop", "停止 Agent", true)?;
    let restart = MenuItem::with_id(app, "restart", "重启 Agent", true)?;
    let settings = MenuItem::with_id(app, "settings", "设置", true)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true)?;

    let menu = Menu::with_items(app, &[&start, &stop, &restart, &settings, &quit])?;

    let icon = app.default_window_icon().cloned().unwrap();

    let _tray = TrayIconBuilder::new()
        .icon(icon)
        .tooltip("DeepSeek Harness")
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "start" => {
                let a = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = crate::sidecar::spawn_dsh(&a).await;
                });
            }
            "stop" => {
                let _ = crate::sidecar::stop_dsh(app);
            }
            "restart" => {
                let a = app.clone();
                tauri::async_runtime::spawn(async move {
                    let _ = crate::sidecar::stop_dsh(&a);
                    let _ = crate::sidecar::spawn_dsh(&a).await;
                });
            }
            "settings" => {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }
            "quit" => {
                let _ = crate::sidecar::stop_dsh(app);
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}
