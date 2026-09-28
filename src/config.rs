//! `config.toml`: where to sync to, and (on the server) how to notify.
//!
//! ```toml
//! [sync]                        # clients
//! server = "https://todo.example.net"
//! token = "long-random-string"
//!
//! [server]                      # `td serve`
//! listen = "0.0.0.0:8787"
//! db = "/var/lib/todont/server.db"
//! token = "long-random-string"
//! timezone = "America/Toronto"  # for times in notifications
//!
//! [ntfy]                        # `td serve`; omit to disable
//! topic = "todont-some-secret-topic"
//! url = "https://ntfy.sh"
//! lead_minutes = 0              # remind this long before the due time
//! nag_minutes = 120             # re-notify overdue tasks; 0 disables
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("{path}: {what}")]
    Missing { path: PathBuf, what: &'static str },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub sync: Option<SyncConfig>,
    pub server: Option<ServerConfig>,
    pub ntfy: Option<NtfyConfig>,
    /// Where this was loaded from, for error messages.
    #[serde(skip)]
    pub path: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub server: String,
    pub token: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub db: PathBuf,
    pub token: String,
    /// IANA zone name; defaults to the server's system zone.
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NtfyConfig {
    pub topic: String,
    #[serde(default = "default_ntfy_url")]
    pub url: String,
    /// Access token for a protected topic on a self-hosted server.
    pub token: Option<String>,
    #[serde(default)]
    pub lead_minutes: u32,
    #[serde(default = "default_nag_minutes")]
    pub nag_minutes: u32,
}

fn default_listen() -> String {
    "127.0.0.1:8787".into()
}

fn default_ntfy_url() -> String {
    "https://ntfy.sh".into()
}

fn default_nag_minutes() -> u32 {
    120
}

impl Config {
    /// `$TODONT_CONFIG`, else `$XDG_CONFIG_HOME/todont/config.toml`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = std::env::var_os("TODONT_CONFIG") {
            return PathBuf::from(p);
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|h| PathBuf::from(h).join(".config"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("todont/config.toml")
    }

    /// Loads `path`; a missing file is an empty config (sync disabled).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Config {
                    path: path.into(),
                    ..Default::default()
                });
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.into(),
                    source,
                });
            }
        };
        let mut config: Config =
            toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.into(),
                source,
            })?;
        config.path = path.into();
        Ok(config)
    }

    pub fn server(&self) -> Result<&ServerConfig, ConfigError> {
        self.server.as_ref().ok_or_else(|| ConfigError::Missing {
            path: self.path.clone(),
            what: "no [server] section; run `td init --server` to add one",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_example_parses() {
        let doc = include_str!("config.rs")
            .lines()
            .skip_while(|l| !l.starts_with("//! ```toml"))
            .skip(1)
            .take_while(|l| !l.starts_with("//! ```"))
            .map(|l| l.trim_start_matches("//!").trim_start())
            .collect::<Vec<_>>()
            .join("\n");
        let c: Config = toml::from_str(&doc).unwrap();
        assert_eq!(c.ntfy.unwrap().nag_minutes, 120);
        assert_eq!(c.server.unwrap().listen, "0.0.0.0:8787");
        assert!(c.sync.is_some());
    }
}
