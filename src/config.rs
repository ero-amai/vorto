use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::Path;
use std::process;

use serde::{Deserialize, Serialize};

use crate::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub tunnels: Vec<TunnelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TunnelConfig {
    pub name: String,
    pub listen: String,
    pub target: String,
    pub protocol: Protocol,
    #[serde(default = "default_tcp_mode")]
    pub tcp_mode: TcpMode,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
    Both,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TcpMode {
    #[default]
    Auto,
    Throughput,
    Latency,
}

fn default_enabled() -> bool {
    true
}

fn default_tcp_mode() -> TcpMode {
    TcpMode::Auto
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            listen: "0.0.0.0:0".to_string(),
            target: "127.0.0.1:0".to_string(),
            protocol: Protocol::Tcp,
            tcp_mode: TcpMode::Auto,
            enabled: true,
        }
    }
}

impl Protocol {
    pub fn supports_tcp(self) -> bool {
        matches!(self, Self::Tcp | Self::Both)
    }

    pub fn supports_udp(self) -> bool {
        matches!(self, Self::Udp | Self::Both)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
            Self::Both => "both",
        }
    }
}

impl TcpMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Throughput => "throughput",
            Self::Latency => "latency",
        }
    }

    pub fn effective(self) -> Self {
        match self {
            Self::Auto => Self::Throughput,
            explicit => explicit,
        }
    }
}

impl AppConfig {
    fn parse(content: &str) -> AppResult<Self> {
        let config = serde_yaml::from_str::<Self>(content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn load_or_default(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let content = fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        Self::parse(&content)
    }

    pub fn load_for_runtime(path: &Path) -> AppResult<Self> {
        if !path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("Config file not found: {}", path.display()),
            )
            .into());
        }

        let content = fs::read_to_string(path)?;
        if content.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Config file is empty: {}", path.display()),
            )
            .into());
        }

        Self::parse(&content)
    }

    pub fn save(&self, path: &Path) -> AppResult<()> {
        self.validate()?;
        let rendered = serde_yaml::to_string(self)?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("config.yaml");
        let temp_path = path.with_file_name(format!(".{}.tmp.{}", file_name, process::id()));
        fs::write(&temp_path, rendered)?;
        fs::rename(&temp_path, path)?;
        Ok(())
    }

    pub fn enabled_tunnels(&self) -> Vec<TunnelConfig> {
        self.tunnels
            .iter()
            .filter(|tunnel| tunnel.enabled)
            .cloned()
            .collect()
    }

    pub fn validate(&self) -> AppResult<()> {
        let mut names = HashSet::new();
        for tunnel in &self.tunnels {
            tunnel.validate()?;
            if !names.insert(tunnel.name.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Duplicate tunnel name: {}", tunnel.name),
                )
                .into());
            }
        }
        Ok(())
    }
}

impl TunnelConfig {
    pub fn validate(&self) -> AppResult<()> {
        if self.name.trim().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Tunnel name cannot be empty.",
            )
            .into());
        }

        self.listen
            .parse::<std::net::SocketAddr>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid listen address {}: {}", self.listen, error),
                )
            })?;

        self.target
            .parse::<std::net::SocketAddr>()
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid target address {}: {}", self.target, error),
                )
            })?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("vorto-{name}-{unique}.yaml"))
    }

    #[test]
    fn load_for_runtime_rejects_missing_file() {
        let path = temp_path("missing");
        let error = AppConfig::load_for_runtime(&path).expect_err("missing file should fail");
        assert!(error.to_string().contains("Config file not found"));
    }

    #[test]
    fn load_for_runtime_rejects_empty_file() {
        let path = temp_path("empty");
        fs::write(&path, "").expect("should write temp file");

        let error = AppConfig::load_for_runtime(&path).expect_err("empty file should fail");
        assert!(error.to_string().contains("Config file is empty"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn load_for_runtime_accepts_explicit_empty_tunnels() {
        let path = temp_path("empty-tunnels");
        fs::write(&path, "tunnels: []\n").expect("should write temp file");

        let config = AppConfig::load_for_runtime(&path).expect("explicit empty config should load");
        assert!(config.tunnels.is_empty());

        let _ = fs::remove_file(path);
    }

    #[test]
    fn save_writes_a_runtime_loadable_config() {
        let path = temp_path("save");
        let config = AppConfig {
            tunnels: vec![TunnelConfig {
                name: "alpha".to_string(),
                listen: "127.0.0.1:18080".to_string(),
                target: "127.0.0.1:8080".to_string(),
                protocol: Protocol::Tcp,
                tcp_mode: TcpMode::Latency,
                enabled: true,
            }],
        };

        config.save(&path).expect("config save should succeed");
        let loaded = AppConfig::load_for_runtime(&path).expect("saved config should load");
        assert_eq!(loaded, config);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn parse_defaults_tcp_mode_to_auto_for_older_configs() {
        let config = AppConfig::parse(
            "tunnels:\n  - name: legacy\n    listen: 127.0.0.1:18080\n    target: 127.0.0.1:8080\n    protocol: tcp\n",
        )
        .expect("older config should still parse");

        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].tcp_mode, TcpMode::Auto);
    }

    #[test]
    fn tcp_mode_auto_defaults_to_throughput_at_runtime() {
        assert_eq!(TcpMode::Auto.effective(), TcpMode::Throughput);
        assert_eq!(TcpMode::Latency.effective(), TcpMode::Latency);
    }
}
