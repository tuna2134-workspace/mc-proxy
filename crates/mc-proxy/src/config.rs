//! TOML configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Configuration errors.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read config file {path}: {source}")]
    Read {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        source: std::io::Error,
    },
    /// The file is not valid TOML.
    #[error("could not parse config file {path}: {source}")]
    Parse {
        /// Path that failed.
        path: PathBuf,
        /// Underlying error.
        source: toml::de::Error,
    },
    /// The configuration is semantically invalid.
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// How server-list pings are answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum StatusMode {
    /// Answer with the proxy's own MOTD and player count.
    #[default]
    Proxy,
    /// Forward the ping to the default backend server.
    Passthrough,
}

/// Ping response settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct StatusConfig {
    /// `proxy` or `passthrough`.
    pub mode: StatusMode,
}

impl Default for StatusConfig {
    fn default() -> Self {
        Self {
            mode: StatusMode::Proxy,
        }
    }
}

/// Timeouts, in milliseconds.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TimeoutConfig {
    /// Time allowed for the handshake and status request.
    pub handshake_ms: u64,
    /// Time allowed for the login exchange with the client.
    pub login_ms: u64,
    /// Time allowed to connect and log in to a backend.
    pub backend_ms: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            handshake_ms: 10_000,
            login_ms: 30_000,
            backend_ms: 30_000,
        }
    }
}

/// Session-server authentication settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// Base URL of the session server (`hasJoined` is appended).
    pub session_server: String,
    /// HTTP timeout in milliseconds.
    pub timeout_ms: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            session_server: "https://sessionserver.mojang.com".to_owned(),
            timeout_ms: 5_000,
        }
    }
}

/// F-Stack/DPDK settings (ignored by the standard backend).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct FstackConfig {
    /// Arguments passed to `ff_init`, e.g.
    /// `["--conf", "/etc/f-stack.conf", "--proc-type=primary"]`.
    pub eal_args: Vec<String>,
}

/// The `[try]` table: servers to try in order when a player connects.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TryConfig {
    /// Server names, in order of preference.
    pub servers: Vec<String>,
}

/// A backend server entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerEntry {
    /// Logical name used by routing and the `try` list.
    pub name: String,
    /// `host:port` of the backend.
    pub address: String,
}

/// The full proxy configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Listen address, e.g. `0.0.0.0:25565`.
    pub bind: String,
    /// MOTD shown in the server list (`&` colour codes are supported).
    pub motd: String,
    /// Maximum player count advertised in the server list.
    pub max_players: usize,
    /// Enable Mojang session authentication.
    pub online_mode: bool,
    /// Number of proxy workers (one reactor + Tokio runtime per worker).
    /// Each worker binds the same address with `SO_REUSEPORT`.
    pub workers: usize,
    /// Pin worker threads to CPU cores (best effort).
    pub pin_cores: bool,
    /// Compression threshold; `-1` disables compression.
    pub compression_threshold: i32,
    /// Shared secret for Velocity modern forwarding. Empty disables it.
    pub forwarding_secret: String,
    /// File to read the forwarding secret from (used when the field above is
    /// empty).
    pub forwarding_secret_file: Option<PathBuf>,
    /// Optional server icon (PNG).
    pub favicon: Option<PathBuf>,
    /// Version name shown in the server list.
    pub version_name: String,
    /// Backend servers; the first one is the default.
    pub servers: Vec<ServerEntry>,
    /// Hostname to server routing table.
    pub forced_hosts: HashMap<String, String>,
    /// Servers to try in order when connecting a player.
    #[serde(rename = "try")]
    pub try_servers: TryConfig,
    /// Ping settings.
    pub status: StatusConfig,
    /// Timeouts.
    pub timeouts: TimeoutConfig,
    /// Authentication settings.
    pub auth: AuthConfig,
    /// F-Stack settings.
    pub fstack: FstackConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:25565".to_owned(),
            motd: "A Minecraft Server".to_owned(),
            max_players: 100,
            online_mode: false,
            workers: 1,
            pin_cores: true,
            compression_threshold: 256,
            forwarding_secret: String::new(),
            forwarding_secret_file: None,
            favicon: None,
            version_name: format!("mc-proxy {}", env!("CARGO_PKG_VERSION")),
            servers: Vec::new(),
            forced_hosts: HashMap::new(),
            try_servers: TryConfig::default(),
            status: StatusConfig::default(),
            timeouts: TimeoutConfig::default(),
            auth: AuthConfig::default(),
            fstack: FstackConfig::default(),
        }
    }
}

impl Config {
    /// Parse a configuration from a TOML string.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: PathBuf::from("<memory>"),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Load a configuration from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_owned(),
            source,
        })?;
        let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Validate cross-field invariants.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.servers.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one [[servers]] entry is required".into(),
            ));
        }
        let mut names = std::collections::HashSet::new();
        for server in &self.servers {
            if !names.insert(server.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate server name '{}'",
                    server.name
                )));
            }
            if server.address.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "server '{}' has an empty address",
                    server.name
                )));
            }
            if server.name.is_empty() {
                return Err(ConfigError::Invalid("server name must not be empty".into()));
            }
        }
        for (host, server) in &self.forced_hosts {
            if !names.contains(server.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "forced host '{host}' points at unknown server '{server}'"
                )));
            }
        }
        for server in &self.try_servers.servers {
            if !names.contains(server.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "try list references unknown server '{server}'"
                )));
            }
        }
        if self.compression_threshold < -1 {
            return Err(ConfigError::Invalid(
                "compression_threshold must be -1 or larger".into(),
            ));
        }
        if self.online_mode && self.auth.session_server.is_empty() {
            return Err(ConfigError::Invalid(
                "online_mode requires an auth.session_server URL".into(),
            ));
        }
        if self.workers == 0 {
            return Err(ConfigError::Invalid("workers must be at least 1".into()));
        }
        if self.max_players == 0 {
            return Err(ConfigError::Invalid("max_players must be positive".into()));
        }
        Ok(())
    }

    /// Resolve the forwarding secret, reading it from a file when configured.
    pub fn resolve_forwarding_secret(&self) -> Result<Vec<u8>, ConfigError> {
        if !self.forwarding_secret.is_empty() {
            return Ok(self.forwarding_secret.as_bytes().to_vec());
        }
        if let Some(path) = &self.forwarding_secret_file {
            let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            return Ok(text.trim().as_bytes().to_vec());
        }
        Ok(Vec::new())
    }

    /// Compression threshold as an `Option` (`-1` = disabled).
    pub fn compression(&self) -> Option<u32> {
        if self.compression_threshold < 0 {
            None
        } else {
            Some(self.compression_threshold as u32)
        }
    }

    /// Handshake timeout.
    pub fn handshake_timeout(&self) -> Duration {
        Duration::from_millis(self.timeouts.handshake_ms)
    }

    /// Login timeout.
    pub fn login_timeout(&self) -> Duration {
        Duration::from_millis(self.timeouts.login_ms)
    }

    /// Backend timeout.
    pub fn backend_timeout(&self) -> Duration {
        Duration::from_millis(self.timeouts.backend_ms)
    }

    /// Load the favicon as a `data:` URI.
    pub fn load_favicon(&self) -> Result<Option<String>, ConfigError> {
        let Some(path) = &self.favicon else {
            return Ok(None);
        };
        let bytes = std::fs::read(path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(ConfigError::Invalid(format!(
                "favicon {} is not a PNG file",
                path.display()
            )));
        }
        if bytes.len() > 64 * 1024 {
            return Err(ConfigError::Invalid(format!(
                "favicon {} is larger than 64 KiB",
                path.display()
            )));
        }
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(Some(format!("data:image/png;base64,{encoded}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_toml() -> &'static str {
        r#"
bind = "127.0.0.1:25565"
motd = "&aHello"
online_mode = false
forwarding_secret = "secret"

[[servers]]
name = "lobby"
address = "127.0.0.1:25566"

[forced_hosts]
"play.example.com" = "lobby"
"#
    }

    #[test]
    fn parses_minimal_config() {
        let config = Config::from_toml(base_toml()).unwrap();
        assert_eq!(config.bind, "127.0.0.1:25565");
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "lobby");
        assert_eq!(config.compression(), Some(256));
        assert_eq!(config.forced_hosts["play.example.com"], "lobby");
    }

    #[test]
    fn rejects_unknown_forced_host_target() {
        let text = base_toml().replace(
            "\"play.example.com\" = \"lobby\"",
            "\"play.example.com\" = \"missing\"",
        );
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("unknown server"));
    }

    #[test]
    fn rejects_empty_servers() {
        let err = Config::from_toml("bind = \"0.0.0.0:25565\"").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn disabled_compression() {
        let text = base_toml().replace(
            "online_mode = false",
            "online_mode = false\ncompression_threshold = -1",
        );
        let config = Config::from_toml(&text).unwrap();
        assert_eq!(config.compression(), None);
    }
}
