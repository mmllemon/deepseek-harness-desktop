# 图标说明

Tauri 打包需要图标资源。请在本目录放置以下文件（可用 `cargo tauri icon <源图>` 一次性生成）：

- `icon.ico`           (Windows 安装包/任务栏)
- `icon.png`           (256x256，源图)
- `icon.ico` 之外的 `icon.[png,icns]`

`.gitignore` 未忽略图标；建议提交 `icon.ico`/`icon.png` 以保证 CI 可复现。

> 注意：`tauri.conf.json` 的 `bundle.icon` 当前指向 `["icons/icon.ico"]`，请确认该文件存在后再执行 `tauri build`。
