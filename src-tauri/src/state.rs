//! 应用状态与配置数据结构（§4.3 / §10.3）。
//! 所有前端可观测字段均通过 Tauri command / event 暴露，密钥不进渲染进程。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub auto_start: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 3080, auto_start: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    /// UI 态；真实密钥存 OS keyring，不在 config.json 持久化明文
    pub provider: String,
    pub model: String,
    pub api_key: String,
    pub base_url: String,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            provider: "deepseek-official".into(),
            model: "deepseek-v4-flash".into(),
            api_key: String::new(),
            base_url: String::new(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathsConfig {
    pub data_dir: String,
    pub log_dir: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdvancedConfig {
    #[serde(default)]
    pub extra_env: HashMap<String, String>,
    pub log_level: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UiConfig {
    pub theme: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdatesConfig {
    pub channel: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub server: ServerConfig,
    pub model: ModelConfig,
    pub paths: PathsConfig,
    pub advanced: AdvancedConfig,
    pub ui: UiConfig,
    pub updates: UpdatesConfig,
}

/// 前端只读的运行态快照（§10.3 IPC 契约）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    pub state: String, // running | stopped | error
    pub agent_port: u16,
    pub proxy_port: u16,
    pub proxy_url: String,
    pub pid: Option<u32>,
}

/// sidecar 进程句柄（持有以避免被 drop 导致子进程退出）。
/// 注：node 由 `std::process::Command` 直接启动（不再经 tauri_plugin_shell 的 sidecar）。
pub struct ChildHandle {
    pub pid: u32,
    pub child: std::process::Child,
    #[cfg(windows)]
    pub job: Option<crate::job::job::JobHandle>,
}

/// 运行期内核状态。
pub struct InnerState {
    pub child: Option<ChildHandle>,
    pub token: Option<String>,
    pub proxy_port: Option<u16>,
    pub proxy_url: Option<String>,
    pub agent_port: Option<u16>,
    pub state: String,
    pub last_error: Option<String>,
}

impl Default for InnerState {
    fn default() -> Self {
        Self {
            child: None,
            token: None,
            proxy_port: None,
            proxy_url: None,
            agent_port: None,
            state: "stopped".into(),
            last_error: None,
        }
    }
}

/// 全局状态：配置 + 运行态，均置于 Mutex 供 Tauri 命令访问。
pub struct AppState {
    pub config: Mutex<AppConfig>,
    pub inner: Mutex<InnerState>,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            config: Mutex::new(AppConfig::default()),
            inner: Mutex::new(InnerState::default()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct LogLine {
    pub stream: String, // stdout | stderr
    pub line: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateEvent {
    pub state: String,
    pub proxy_url: String,
    pub agent_port: u16,
    pub proxy_port: u16,
    pub pid: Option<u32>,
}
