// Author: Passwd Word
//! Muse-Api 配置: 环境变量读入。
//!
//! 优先级 (已核实 / 待确认见 README):
//! - 已核实 (remote_import.rs 直读): `M2RS_DATA_DIR`, `M2RS_FULL_BROWSER_BIN`, `M2RS_XVFB_BIN`
//! - 其余按 `M2RS_*` 优先, `MUSE2API_*` (Python 原版同名项) 次之, `MAPI_*` / `PORT` 兼容。
//! - 端口默认 18611 (与 Python 版 18610 错开, 可同机共存)。
use std::path::PathBuf;

fn env_first(keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| std::env::var(k).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_int(keys: &[&str], default: i64) -> i64 {
    env_first(keys)
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
}

fn env_bool(keys: &[&str], default: bool) -> bool {
    match env_first(keys).as_deref() {
        Some("1") | Some("true") | Some("yes") => true,
        Some("0") | Some("false") | Some("no") => false,
        _ => default,
    }
}

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_dir: PathBuf,
    pub data_dir: PathBuf,
    pub host: String,
    pub port: u16,
    pub api_key: String,
    pub chromium: String,
    pub full_browser_bin: String,
    pub xvfb_bin: String,
    pub cdp_port: u16,
    pub site_url: String,
    pub profile_root: PathBuf,
    pub public_base: String,
    pub image_timeout: u64,
    pub video_timeout: u64,
    pub chat_timeout: u64,
    pub max_concurrency: usize,
    pub login_wait: u64,
    pub tool_protocol: bool,
    pub version: String,
}

impl Config {
    pub fn from_env() -> Self {
        let base_dir = env_first(&["M2RS_HOME", "MUSE2API_HOME", "MAPI_HOME"])
            .map(PathBuf::from)
            .unwrap_or_else(exe_dir);
        let data_dir = env_first(&["M2RS_DATA_DIR", "MUSE2API_DATA_DIR"])
            .map(PathBuf::from)
            .unwrap_or_else(|| base_dir.join("data"));
        let chromium = env_first(&["M2RS_CHROMIUM", "MUSE2API_CHROMIUM"])
            .unwrap_or_else(detect_chromium);
        let full_browser_bin = env_first(&["M2RS_FULL_BROWSER_BIN"])
            .unwrap_or_else(|| chromium.clone());
        let xvfb_bin =
            env_first(&["M2RS_XVFB_BIN"]).unwrap_or_else(|| "Xvfb".to_string());
        let api_key = env_first(&["M2RS_API_KEY", "MUSE2API_KEY", "MAPI_KEY"])
            .unwrap_or_default();
        let profile_root = env_first(&["M2RS_PROFILE_ROOT", "MUSE2API_PROFILE_ROOT"])
            .map(PathBuf::from)
            .unwrap_or_else(|| home_dir().join("muse-api-profiles"));
        Config {
            base_dir,
            data_dir,
            host: env_first(&["M2RS_HOST", "MUSE2API_HOST"]).unwrap_or_else(|| "0.0.0.0".to_string()),
            port: env_int(&["M2RS_PORT", "MUSE2API_PORT", "MAPI_PORT", "PORT"], 18611) as u16,
            api_key,
            chromium,
            full_browser_bin,
            xvfb_bin,
            cdp_port: env_int(&["M2RS_CDP_PORT", "MUSE2API_CDP_PORT"], 19211) as u16,
            site_url: env_first(&["M2RS_SITE", "MUSE2API_SITE"])
                .unwrap_or_else(|| "https://muse.ai/".to_string()),
            profile_root,
            public_base: env_first(&["M2RS_PUBLIC_BASE", "MUSE2API_PUBLIC_BASE"])
                .unwrap_or_default(),
            image_timeout: env_int(&["M2RS_IMAGE_TIMEOUT", "MUSE2API_IMAGE_TIMEOUT"], 240) as u64,
            video_timeout: env_int(&["M2RS_VIDEO_TIMEOUT", "MUSE2API_VIDEO_TIMEOUT"], 600) as u64,
            chat_timeout: env_int(&["M2RS_CHAT_TIMEOUT", "MUSE2API_CHAT_TIMEOUT"], 300) as u64,
            max_concurrency: env_int(&["M2RS_CONCURRENCY", "MUSE2API_CONCURRENCY"], 8).max(1) as usize,
            login_wait: env_int(&["M2RS_LOGIN_WAIT", "MUSE2API_LOGIN_WAIT"], 60) as u64,
            tool_protocol: env_bool(&["M2RS_TOOL_PROTOCOL", "MUSE2API_TOOL_PROTOCOL"], false),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    pub fn media_dir(&self) -> PathBuf {
        self.data_dir.join("media")
    }

    pub fn download_dir(&self) -> PathBuf {
        self.data_dir.join("downloads")
    }

    pub fn profile_dir(&self) -> PathBuf {
        self.profile_root.join("generate")
    }

    pub fn accounts_file(&self) -> PathBuf {
        self.data_dir.join("accounts.json")
    }

    pub fn tasks_file(&self) -> PathBuf {
        self.data_dir.join("tasks.json")
    }

    pub fn settings_file(&self) -> PathBuf {
        self.data_dir.join("settings.json")
    }

    pub fn executions_file(&self) -> PathBuf {
        self.data_dir.join("executions.json")
    }

    pub fn ensure_dirs(&self) {
        for d in [
            &self.base_dir,
            &self.data_dir,
            &self.media_dir(),
            &self.download_dir(),
            &self.profile_dir(),
        ] {
            let _ = std::fs::create_dir_all(d);
        }
    }
}

fn detect_chromium() -> String {
    for c in [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "chromium",
        "chromium-browser",
        "google-chrome",
        "chrome",
    ] {
        if std::path::Path::new(c).is_file() || which(c).is_some() {
            return c.to_string();
        }
    }
    "chromium".to_string()
}

fn which(name: &str) -> Option<PathBuf> {
    let out = std::process::Command::new("which").arg(name).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(PathBuf::from)
}

/// 加载 .env (项目根目录, 不覆盖已存在的环境变量; Python config.py 同款行为)。
pub fn load_dotenv(base_dir: &std::path::Path) {
    let path = base_dir.join(".env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let s = line.trim();
        if s.is_empty() || s.starts_with('#') || !s.contains('=') {
            continue;
        }
        if let Some((k, v)) = s.split_once('=') {
            let k = k.trim();
            let v = v.trim().trim_matches(|c| c == '\'' || c == '"');
            if !k.is_empty() && std::env::var(k).is_err() {
                std::env::set_var(k, v);
            }
        }
    }
}
