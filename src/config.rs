use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub network_id: String,
    #[serde(default)]
    pub tls: TlsConfig,
    pub archive: ArchiveConfig,
    pub injector: InjectorConfig,
    pub peers: PeersConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub http: HttpConfig,

    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TlsConfig {
    #[serde(default = "default_tls_cert_path")]
    pub cert_path: PathBuf,
    #[serde(default = "default_tls_key_path")]
    pub key_path: PathBuf,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            cert_path: default_tls_cert_path(),
            key_path: default_tls_key_path(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveConfig {
    /// Path to the local proof archive SQLite DB (produced by `bbr_scrap`).
    pub db_path: PathBuf,

    /// SQLite busy timeout (ms) for read operations.
    #[serde(default = "default_archive_busy_timeout_ms")]
    pub busy_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InjectorConfig {
    /// Default send budget on first launch (used to initialize in-flight state).
    pub default_proofs_per_window: u32,

    /// Default window size (ms) on first launch (used to initialize in-flight state).
    pub default_window_ms: u64,

    /// How often to poll the local archive DB event log.
    pub poll_interval_ms: u64,

    /// Batch size for reading from the local archive DB.
    pub events_read_limit: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeersConfig {
    /// Fixed list of FULL_NODE peers to inject into.
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StorageConfig {
    #[serde(default = "default_in_flight_path")]
    pub in_flight_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_http_listen_addr")]
    pub listen_addr: String,
    #[serde(default = "default_static_dir")]
    pub static_dir: PathBuf,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_http_listen_addr(),
            static_dir: default_static_dir(),
        }
    }
}

impl Config {
    pub async fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read config {}", path.display()))?;
        let mut cfg: Self = toml::from_str(&raw).context("parse config TOML")?;

        let base_dir = path
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        cfg.tls.cert_path = absolutize(&base_dir, &cfg.tls.cert_path);
        cfg.tls.key_path = absolutize(&base_dir, &cfg.tls.key_path);
        cfg.archive.db_path = absolutize(&base_dir, &cfg.archive.db_path);
        cfg.storage.in_flight_path = absolutize(&base_dir, &cfg.storage.in_flight_path);
        cfg.http.static_dir = absolutize(&base_dir, &cfg.http.static_dir);

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> anyhow::Result<()> {
        if self.network_id.trim().is_empty() {
            return Err(anyhow!("network_id must not be empty"));
        }
        if self.archive.db_path.as_os_str().is_empty() {
            return Err(anyhow!("archive.db_path must not be empty"));
        }
        if self.injector.default_proofs_per_window == 0 {
            return Err(anyhow!("injector.default_proofs_per_window must be > 0"));
        }
        if self.injector.default_window_ms == 0 {
            return Err(anyhow!("injector.default_window_ms must be > 0"));
        }
        if self.injector.poll_interval_ms == 0 {
            return Err(anyhow!("injector.poll_interval_ms must be > 0"));
        }
        if self.injector.events_read_limit == 0 {
            return Err(anyhow!("injector.events_read_limit must be > 0"));
        }
        if self.peers.targets.is_empty() {
            return Err(anyhow!("peers.targets must not be empty"));
        }
        if self.storage.in_flight_path.as_os_str().is_empty() {
            return Err(anyhow!("storage.in_flight_path must not be empty"));
        }
        let _listen: std::net::SocketAddr = self
            .http
            .listen_addr
            .parse()
            .context("http.listen_addr must be a valid socket address")?;
        Ok(())
    }
}

fn absolutize(base_dir: &Path, p: &Path) -> PathBuf {
    let p = expand_tilde(p);
    if p.is_absolute() {
        return p;
    }
    base_dir.join(p)
}

fn expand_tilde(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_path_buf();
    };
    if (s == "~" || s.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let home = PathBuf::from(home);
        if s == "~" {
            return home;
        }
        if let Some(rest) = s.strip_prefix("~/") {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

fn default_tls_cert_path() -> PathBuf {
    PathBuf::from("~/.chia/mainnet/config/ssl/full_node/public_full_node.crt")
}

fn default_tls_key_path() -> PathBuf {
    PathBuf::from("~/.chia/mainnet/config/ssl/full_node/public_full_node.key")
}

fn default_http_listen_addr() -> String {
    "127.0.0.1:6182".to_string()
}

fn default_static_dir() -> PathBuf {
    PathBuf::from("ui/dist")
}

fn default_in_flight_path() -> PathBuf {
    PathBuf::from("data/in_flight.json")
}

fn default_archive_busy_timeout_ms() -> u64 {
    500
}
