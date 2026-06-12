use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::client::ServerConfig;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    #[serde(default)]
    pub active_server: Option<String>,
    #[serde(default)]
    pub active_database: Option<String>,
    #[serde(default)]
    pub active_modules_database: Option<String>,
    #[serde(default)]
    pub active_app_server: Option<String>,
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        let dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("marklogic-tui");
        fs::create_dir_all(&dir).ok();
        dir.join("config.toml")
    }

    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if path.exists() {
            let content = fs::read_to_string(&path)?;
            let mut config: AppConfig = toml::from_str(&content)?;
            config.migrate_legacy_servers();
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    fn migrate_legacy_servers(&mut self) {
        for server in &mut self.servers {
            if let Some(ref uri) = server.uri {
                if server.host.is_empty() {
                    if let Ok(parsed) = url::Url::parse(uri) {
                        server.host = parsed.host_str().unwrap_or(uri).to_string();
                        server.secure = parsed.scheme() == "https";
                    } else {
                        server.host = uri.clone();
                        server.secure = false;
                    }
                }
                // Create default app server from old port if list is empty
                if server.app_servers.is_empty() {
                    let default_name = format!("app-{}", server.port);
                    server.app_servers.push(crate::client::AppServerEndpoint {
                        name: default_name,
                        port: server.port,
                        secure: server.secure,
                        content_database: None,
                        modules_database: None,
                    });
                }
                server.uri = None;
            }
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        let content = toml::to_string_pretty(self)?;

        // Validate: try to parse what we just serialized
        if let Err(e) = toml::from_str::<AppConfig>(&content) {
            bail!("Generated invalid TOML, refusing to write: {}", e);
        }

        // Write to temp file first, then rename atomically
        let tmp_path = path.with_extension("toml.tmp");
        fs::write(&tmp_path, &content)?;
        fs::rename(&tmp_path, &path)?;

        Ok(())
    }

    pub fn active_server_config(&self) -> Option<&ServerConfig> {
        self.active_server
            .as_ref()
            .and_then(|name| self.servers.iter().find(|s| &s.name == name))
    }

    /// Returns the effective `ServerConfig` for the active server, applying the
    /// currently selected app server's port and secure flag when available.
    pub fn effective_active_server_config(&self) -> Option<ServerConfig> {
        let mut server = self.active_server_config()?.clone();
        if let Some(active_name) = self.active_app_server.as_deref()
            && let Some(app_server) = server.app_servers.iter().find(|a| a.name == active_name)
        {
            server.port = app_server.port;
            server.secure = app_server.secure;
        }
        Some(server)
    }

    pub fn add_server(&mut self, server: ServerConfig) {
        self.servers.retain(|s| s.name != server.name);
        self.active_server = Some(server.name.clone());
        self.servers.push(server);
    }

    pub fn remove_server(&mut self, name: &str) {
        self.servers.retain(|s| s.name != name);
        if self.active_server.as_deref() == Some(name) {
            self.active_server = self.servers.first().map(|s| s.name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppConfig;
    use crate::client::ServerConfig;

    fn server(name: &str, host: &str) -> ServerConfig {
        use crate::client::AuthType;
        ServerConfig {
            name: name.to_string(),
            host: host.to_string(),
            username: "admin".to_string(),
            password: "admin".to_string(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        }
    }

    #[test]
    fn add_server_makes_new_server_active() {
        let mut config = AppConfig {
            servers: vec![server("old", "old.example")],
            active_server: Some("old".to_string()),
            active_database: Some("Documents".to_string()),
            active_modules_database: Some("Modules".to_string()),
            active_app_server: Some("App-Services".to_string()),
        };

        config.add_server(server("new", "new.example"));

        assert_eq!(config.active_server.as_deref(), Some("new"));
        assert_eq!(config.servers.len(), 2);
    }

    #[test]
    fn replacing_server_keeps_replacement_active() {
        let mut config = AppConfig {
            servers: vec![server("local", "old.example")],
            active_server: Some("local".to_string()),
            active_database: None,
            active_modules_database: None,
            active_app_server: None,
        };

        config.add_server(server("local", "new.example"));

        assert_eq!(config.active_server.as_deref(), Some("local"));
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "new.example");
    }

    #[test]
    fn effective_active_server_config_prefers_active_app_server() {
        let mut server = server("local", "localhost");
        server.app_servers = vec![
            crate::client::AppServerEndpoint {
                name: "App-Services".to_string(),
                port: 8000,
                secure: false,
                content_database: None,
                modules_database: None,
            },
            crate::client::AppServerEndpoint {
                name: "custom".to_string(),
                port: 8010,
                secure: true,
                content_database: None,
                modules_database: None,
            },
        ];
        let config = AppConfig {
            servers: vec![server],
            active_server: Some("local".to_string()),
            active_database: None,
            active_modules_database: None,
            active_app_server: Some("custom".to_string()),
        };

        let effective = config.effective_active_server_config().expect("should resolve");
        assert_eq!(effective.port, 8010);
        assert!(effective.secure);
    }
}
