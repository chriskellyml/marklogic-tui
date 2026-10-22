use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::client::ServerConfig;
use crate::tracked_folder::TrackedFolderStore;

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
    /// Values substituted into queries as `%%NAME%%` at execution time.
    #[serde(default)]
    pub active_variables: BTreeMap<String, String>,
}

impl AppConfig {
    pub fn config_path() -> PathBuf {
        #[cfg(test)]
        {
            let dir = test_storage_dir().join("config");
            fs::create_dir_all(&dir).expect("create isolated test config directory");
            dir.join("config.toml")
        }
        #[cfg(not(test))]
        {
            let dir = dirs::config_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("marklogic-tui");
            fs::create_dir_all(&dir).ok();
            dir.join("config.toml")
        }
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

pub const CONFIG_BUNDLE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigBundle {
    #[serde(default = "default_bundle_version")]
    pub version: u32,
    #[serde(default)]
    pub config: AppConfig,
    #[serde(default)]
    pub tracked_folders: TrackedFolderStore,
}

fn default_bundle_version() -> u32 {
    CONFIG_BUNDLE_VERSION
}

impl ConfigBundle {
    pub fn current() -> Result<Self> {
        Ok(Self {
            version: CONFIG_BUNDLE_VERSION,
            config: AppConfig::load()?,
            tracked_folders: TrackedFolderStore::load()?,
        })
    }

    pub fn export_to(&self, path: &Path) -> Result<()> {
        let content =
            serde_yaml::to_string(self).context("Failed to serialize configuration bundle")?;

        if let Err(e) = serde_yaml::from_str::<ConfigBundle>(&content) {
            bail!("Generated invalid YAML, refusing to write: {}", e);
        }

        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create export directory: {}", parent.display())
            })?;
        }

        let tmp_path = path.with_extension("yaml.tmp");
        fs::write(&tmp_path, &content)
            .with_context(|| format!("Failed to write export: {}", tmp_path.display()))?;
        fs::rename(&tmp_path, path)
            .with_context(|| format!("Failed to finalize export: {}", path.display()))?;

        Ok(())
    }

    pub fn import_from(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config bundle: {}", path.display()))?;
        let mut bundle: ConfigBundle = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse config bundle: {}", path.display()))?;

        if bundle.version > CONFIG_BUNDLE_VERSION {
            bail!(
                "Config bundle version {} is newer than supported version {}",
                bundle.version,
                CONFIG_BUNDLE_VERSION
            );
        }

        bundle.config.migrate_legacy_servers();
        bundle.config.save()?;
        bundle.tracked_folders.save()?;
        bundle.export_to(path)?;

        Ok(bundle)
    }
}

// Unit tests exercise settings mutations through App. Keep their writes out of
// the user's live settings and separate parallel test threads.
#[cfg(test)]
pub(crate) fn test_storage_dir() -> PathBuf {
    std::env::temp_dir().join(format!(
        "marklogic-tui-tests-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ))
}

#[cfg(test)]
mod tests {
    use super::{AppConfig, CONFIG_BUNDLE_VERSION, ConfigBundle};
    use crate::client::ServerConfig;
    use std::collections::BTreeMap;

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
            active_variables: BTreeMap::new(),
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
            active_variables: BTreeMap::new(),
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
            active_variables: BTreeMap::new(),
        };

        let effective = config
            .effective_active_server_config()
            .expect("should resolve");
        assert_eq!(effective.port, 8010);
        assert!(effective.secure);
    }

    #[test]
    fn config_bundle_yaml_round_trips() {
        let bundle = ConfigBundle {
            version: CONFIG_BUNDLE_VERSION,
            config: AppConfig {
                servers: vec![server("local", "localhost")],
                active_server: Some("local".to_string()),
                active_database: Some("Documents".to_string()),
                active_modules_database: None,
                active_app_server: None,
                active_variables: BTreeMap::from([("LIMIT".to_string(), "25".to_string())]),
            },
            tracked_folders: Default::default(),
        };

        let yaml = serde_yaml::to_string(&bundle).expect("serialize");
        let parsed: ConfigBundle = serde_yaml::from_str(&yaml).expect("parse");

        assert_eq!(parsed.version, CONFIG_BUNDLE_VERSION);
        assert_eq!(parsed.config.servers.len(), 1);
        assert_eq!(parsed.config.servers[0].name, "local");
        assert_eq!(parsed.config.active_database.as_deref(), Some("Documents"));
        assert_eq!(
            parsed.config.active_variables.get("LIMIT"),
            Some(&"25".to_string())
        );
    }

    #[test]
    fn config_bundle_defaults_version_and_sections() {
        let yaml = "config:\n  active_database: Documents\n";
        let parsed: ConfigBundle = serde_yaml::from_str(yaml).expect("parse");

        assert_eq!(parsed.version, CONFIG_BUNDLE_VERSION);
        assert!(parsed.config.servers.is_empty());
        assert_eq!(parsed.config.active_database.as_deref(), Some("Documents"));
        assert!(parsed.tracked_folders.folders.is_empty());
    }
}
