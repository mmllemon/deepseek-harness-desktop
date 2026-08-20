//! 配置管理（§4.3 / §10.2 / §10.7 / §12.5）。
//! - 桌面壳配置落 `config.json`（无明文密钥）。
//! - 模型/provider 翻译为 `$DSH_HOME/settings.yaml`（上游热加载机制）。
//! - API Key 仅经 OS keyring 持久化，spawn 时以环境变量注入子进程。

use std::collections::HashMap;
use std::path::PathBuf;

use tauri::Manager;

use crate::state::AppConfig;

const KEYRING_SVC: &str = "deepseek-harness-desktop";
const KEYRING_USER: &str = "api_key";

pub fn config_dir(app: &tauri::AppHandle) -> PathBuf {
    app.path()
        .app_config_dir()
        .expect("无法获取应用配置目录")
}

pub fn config_path(app: &tauri::AppHandle) -> PathBuf {
    config_dir(app).join("config.json")
}

pub fn load_config(app: &tauri::AppHandle) -> AppConfig {
    let p = config_path(app);
    if let Ok(s) = std::fs::read_to_string(&p) {
        if let Ok(cfg) = serde_json::from_str::<AppConfig>(&s) {
            return cfg;
        }
    }
    AppConfig::default()
}

pub fn save_config(app: &tauri::AppHandle, cfg: &AppConfig) -> Result<(), String> {
    let dir = config_dir(app);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let s = serde_json::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    std::fs::write(config_path(app), s).map_err(|e| e.to_string())
}

/// 解析 $DSH_HOME：优先用配置项，否则回退到应用数据目录下的 home 子目录。
pub fn resolve_dsh_home(app: &tauri::AppHandle, cfg: &AppConfig) -> PathBuf {
    if !cfg.paths.data_dir.is_empty() {
        PathBuf::from(&cfg.paths.data_dir)
    } else {
        app.path()
            .app_data_dir()
            .expect("无法获取应用数据目录")
            .join("home")
    }
}

/// 将模型/provider 翻译写入 `$DSH_HOME/settings.yaml`（Cordis 补丁机制，热加载生效）。
/// 采用**合并写入**策略：先读取现有文件保留用户通过 UI 设置的配置，
/// 仅覆盖我们管理的三个命名空间（llm-deepseek / agent-default-model / llm-pi-ai）。
pub fn write_settings_yaml(home: &PathBuf, cfg: &AppConfig) -> Result<(), String> {
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;

    // 读取现有配置（允许合并，保留用户通过 UI 设置的模型自定义提供方等）
    let existing = home.join("settings.yaml");
    let mut root: serde_yaml::Value = if existing.exists() {
        let s = std::fs::read_to_string(&existing).unwrap_or_default();
        serde_yaml::from_str(&s).unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
    } else {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    };

    // 确保 root 是 Mapping
    if !root.is_mapping() {
        root = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }
    let root_map = root.as_mapping_mut().unwrap();

    // llm-deepseek：baseURL（仅当配置了自定义 base_url 时覆盖）
    let mut llm_deepseek = serde_yaml::Mapping::new();
    if !cfg.model.base_url.is_empty() {
        llm_deepseek.insert(
            serde_yaml::Value::from("baseURL"),
            serde_yaml::Value::from(cfg.model.base_url.clone()),
        );
    }
    root_map.insert(
        serde_yaml::Value::from("llm-deepseek"),
        serde_yaml::Value::from(llm_deepseek),
    );

    // agent-default-model：provider + model
    let mut adm = serde_yaml::Mapping::new();
    adm.insert(
        serde_yaml::Value::from("provider"),
        serde_yaml::Value::from(cfg.model.provider.clone()),
    );
    adm.insert(
        serde_yaml::Value::from("model"),
        serde_yaml::Value::from(cfg.model.model.clone()),
    );
    root_map.insert(
        serde_yaml::Value::from("agent-default-model"),
        serde_yaml::Value::from(adm),
    );

    // llm-pi-ai：预置 openai 兼容 provider（仅当我们没有自定义时保留默认值）
    // 用户可通过 UI 添加更多 provider，此处不覆盖整个数组，仅保证默认条目存在
    // 注意：dsh 内部用 replace() 写入，数组顺序由用户决定，此处不触碰

    let s = serde_yaml::to_string(&root).map_err(|e| e.to_string())?;
    std::fs::write(home.join("settings.yaml"), s).map_err(|e| e.to_string())
}

/// 从 `$DSH_HOME/settings.yaml` 读取已保存的主题偏好（用于 WebView localStorage 注入）。
/// 返回 theme id，如 "angelina-dark"；若未设置或读取失败返回 None。
pub fn read_theme_preference(home: &PathBuf) -> Option<String> {
    let existing = home.join("settings.yaml");
    if !existing.exists() {
        return None;
    }
    let s = std::fs::read_to_string(&existing).ok()?;
    let root: serde_yaml::Value = serde_yaml::from_str(&s).ok()?;
    let mapping = root.as_mapping()?;
    let theme_ns = mapping.get(&serde_yaml::Value::from("ui-theme"))?;
    let theme_map = theme_ns.as_mapping()?;
    let pref = theme_map.get(&serde_yaml::Value::from("preference"))?;
    pref.as_str().map(|s| s.to_string())
}

// ---------------- keyring（API Key 持久化，不落明文文件） ----------------

pub fn set_api_key(key: &str) -> Result<(), String> {
    if key.is_empty() {
        return Ok(());
    }
    let entry = keyring::Entry::new(KEYRING_SVC, KEYRING_USER).map_err(|e| e.to_string())?;
    entry.set_password(key).map_err(|e| e.to_string())
}

pub fn get_api_key() -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SVC, KEYRING_USER).ok()?;
    entry.get_password().ok().filter(|k| !k.is_empty())
}

pub fn clear_api_key() {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SVC, KEYRING_USER) {
        let _ = entry.delete_credential();
    }
}

/// 构造 spawn `dsh` 子进程的环境变量（密钥经 env 注入，绝不落明文文件，见 §10.7）。
pub fn build_env(cfg: &AppConfig, home: &PathBuf) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("DSH_HOME".into(), home.to_string_lossy().into_owned());
    env.insert("DSH_TELEMETRY_DISABLED".into(), "1".into());
    if let Some(k) = get_api_key() {
        if !k.is_empty() {
            env.insert("DEEPSEEK_API_KEY".into(), k);
        }
    }
    if !cfg.model.base_url.is_empty() {
        env.insert("DEEPSEEK_BASE_URL".into(), cfg.model.base_url.clone());
    }
    for (k, v) in &cfg.advanced.extra_env {
        env.insert(k.clone(), v.clone());
    }
    env
}
