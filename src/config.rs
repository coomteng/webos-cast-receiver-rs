//! Configuration from environment, with the persistent device UUID.
//! Everything has a default so a bare `cargo run` works on a laptop.
//! Same variables and same `DATA_DIR/uuid` file format as the Node version,
//! so the two can share a data directory (phones key their device cache on
//! the UUID).
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyMode {
    Always,
    HttpsOnly,
    Off,
}

impl ProxyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ProxyMode::Always => "always",
            ProxyMode::HttpsOnly => "https-only",
            ProxyMode::Off => "off",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub bind_addr: String,
    pub advertise_host: String,
    pub base: String,
    pub friendly_name: String,
    pub manufacturer: String,
    pub model_name: String,
    pub uuid: String,
    pub data_dir: PathBuf,
    pub proxy_mode: ProxyMode,
    pub proxy_user_agent: String,
    pub tv_timeout_ms: u64,
    pub app_dir: PathBuf,
    pub sim_dir: PathBuf,
    pub verbose: bool,
}

/// First non-internal IPv4 address; prefers an interface whose name does not
/// look like docker/bridge/virtual. Used for LOCATION and proxy URLs when
/// ADVERTISE_HOST is not set.
pub fn detect_host() -> String {
    let Ok(mut ifs) = if_addrs::get_if_addrs() else { return "127.0.0.1".into() };
    let bad = |n: &str| n.starts_with("docker") || n.starts_with("br-") || n.starts_with("veth") || n.starts_with("virbr") || n.starts_with("lo");
    ifs.sort_by_key(|i| bad(&i.name));
    ifs.iter()
        .find(|i| !i.is_loopback() && i.ip().is_ipv4())
        .map(|i| i.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".into())
}

fn load_uuid(data_dir: &Path) -> String {
    let file = data_dir.join("uuid");
    if let Ok(v) = std::fs::read_to_string(&file) {
        let v = v.trim();
        if v.len() == 36 && v.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return v.to_string();
        }
    }
    let v = uuid::Uuid::new_v4().to_string();
    if let Err(e) = std::fs::create_dir_all(data_dir).and_then(|_| std::fs::write(&file, format!("{v}\n"))) {
        eprintln!("[config] cannot persist uuid in {} - {e}", data_dir.display());
    }
    v
}

/// Default static dirs: `<exe dir>/app`, `<cwd>/app`, or the sibling Node
/// checkout `../webos-cast-receiver/app` (single source of truth in dev).
fn default_static_dir(name: &str) -> PathBuf {
    let mut candidates = vec![PathBuf::from(name)];
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            candidates.push(d.join(name));
        }
    }
    candidates.push(PathBuf::from("../webos-cast-receiver").join(name));
    candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../webos-cast-receiver").join(name));
    candidates.into_iter().find(|p| p.join("index.html").is_file()).unwrap_or_else(|| PathBuf::from(name))
}

impl Config {
    pub fn from_env() -> Self {
        Self::from_map(std::env::vars().collect())
    }

    pub fn from_map(env: HashMap<String, String>) -> Self {
        let get = |k: &str| env.get(k).filter(|v| !v.is_empty()).cloned();
        let data_dir = get("DATA_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("data"));
        let port: u16 = get("PORT").and_then(|p| p.parse().ok()).unwrap_or(8790);
        let bind_addr = get("BIND_ADDR").unwrap_or_default();
        let advertise_host = get("ADVERTISE_HOST")
            .or_else(|| if bind_addr.is_empty() { None } else { Some(bind_addr.clone()) })
            .unwrap_or_else(detect_host);
        let proxy_mode = match get("PROXY_MODE").as_deref() {
            Some("https-only") => ProxyMode::HttpsOnly,
            Some("off") => ProxyMode::Off,
            _ => ProxyMode::Always,
        };
        Config {
            port,
            base: format!("http://{advertise_host}:{port}"),
            bind_addr,
            advertise_host,
            friendly_name: get("FRIENDLY_NAME").unwrap_or_else(|| "客厅电视".into()),
            manufacturer: get("MANUFACTURER").unwrap_or_else(|| "LG Electronics".into()),
            model_name: get("MODEL_NAME").unwrap_or_else(|| "LG TV Cast Receiver".into()),
            uuid: get("UUID").unwrap_or_else(|| load_uuid(&data_dir)),
            data_dir,
            proxy_mode,
            proxy_user_agent: get("PROXY_UA").unwrap_or_else(|| {
                "Mozilla/5.0 (Linux; Android 12; Pixel 6) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36".into()
            }),
            tv_timeout_ms: get("TV_TIMEOUT_MS").and_then(|v| v.parse().ok()).unwrap_or(12_000),
            app_dir: get("APP_DIR").map(PathBuf::from).unwrap_or_else(|| default_static_dir("app")),
            sim_dir: get("SIM_DIR").map(PathBuf::from).unwrap_or_else(|| default_static_dir("sim")),
            verbose: get("VERBOSE").as_deref() == Some("1"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn uuid_persists() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_uuid(dir.path());
        let b = load_uuid(dir.path());
        assert_eq!(a, b);
        assert_eq!(a.len(), 36);
    }
}
