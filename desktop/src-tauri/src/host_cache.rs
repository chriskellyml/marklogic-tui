use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use marklogic_core::{client::ServerConfig, config::AppConfig};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capture<T> {
    pub captured_at: DateTime<Utc>,
    pub data: T,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_warning: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadError {
    pub offline: bool,
    pub message: String,
}

impl From<anyhow::Error> for ReadError {
    fn from(error: anyhow::Error) -> Self {
        let offline = error.chain().any(|cause| {
            cause
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|e| e.is_connect() || e.is_timeout())
                || cause.is::<tokio::time::error::Elapsed>()
        });
        Self {
            offline,
            message: format!("{error:#}"),
        }
    }
}

pub struct HostCache(PathBuf);

impl HostCache {
    pub fn new(profile: &ServerConfig, dataset: &str) -> Self {
        let root = AppConfig::config_path().with_file_name("host-cache-v1");
        Self::at(&root, profile, dataset)
    }

    fn at(root: &Path, profile: &ServerConfig, dataset: &str) -> Self {
        // Share cluster data across app-server ports and databases. A different
        // host, transport or authenticated identity must never reuse it.
        let decoded = profile
            .host
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let mut identity = vec![
            serde_json::json!(decoded),
            serde_json::json!(profile.secure),
            serde_json::json!(profile.insecure),
            serde_json::json!(profile.username),
            serde_json::json!(profile.auth_type),
        ];
        if profile.cluster.manage != Default::default()
            || profile.cluster.query != Default::default()
            || profile.cluster.auth_type.is_some()
        {
            identity.push(serde_json::json!([
                profile.cluster.manage,
                profile.cluster.query,
                profile.cluster.auth_type
            ]));
        }
        let key = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&identity).expect("serializable host identity"))
        );
        Self(root.join(key).join(format!("{dataset}.json")))
    }

    pub fn load<T: DeserializeOwned>(&self) -> Result<Option<Capture<T>>> {
        let file = match fs::File::open(&self.0) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= MAX_BYTES, "Local capture exceeds 8 MiB");
        Ok(Some(
            serde_json::from_slice(&bytes).context("Could not read local capture")?,
        ))
    }

    pub async fn refresh<T: Serialize>(
        &self,
        timeout: Duration,
        read: impl Future<Output = Result<T>>,
    ) -> Result<Capture<T>, ReadError> {
        let started_at = Utc::now();
        let data = tokio::time::timeout(timeout, read)
            .await
            .context("MarkLogic did not respond in time")??;
        let mut capture = Capture {
            captured_at: started_at,
            data,
            cache_warning: None,
        };
        // Concurrent reads can share a host. Keep the last successful request
        // and never let an older request overwrite a newer capture.
        if let Err(error) = self.save(&capture) {
            capture.cache_warning = Some(error.to_string());
        }
        Ok(capture)
    }

    fn save<T: Serialize>(&self, capture: &Capture<T>) -> Result<()> {
        static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let bytes = serde_json::to_vec(capture)?;
        ensure!(
            bytes.len() <= MAX_BYTES,
            "Capture exceeds the 8 MiB local limit"
        );
        let _write = WRITE
            .lock()
            .map_err(|_| anyhow::anyhow!("Local capture store is unavailable"))?;
        if let Ok(Some(existing)) = self.load::<serde::de::IgnoredAny>()
            && existing.captured_at > capture.captured_at
        {
            return Ok(());
        }
        let parent = self.0.parent().context("Invalid local capture path")?;
        fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut file, &bytes)?;
        file.persist(&self.0)
            .context("Could not save local capture")?;
        Ok(())
    }
}

pub fn profile(config: &AppConfig, name: &str) -> Result<ServerConfig> {
    config
        .servers
        .iter()
        .find(|server| server.name == name)
        .cloned()
        .context("Saved server profile no longer exists. Reload the workspace.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use marklogic_core::client::AuthType;

    fn profile() -> ServerConfig {
        ServerConfig {
            name: "Local".into(),
            host: "localhost".into(),
            port: 8000,
            username: "operator".into(),
            password: "password".into(),
            auth_type: AuthType::Basic,
            secure: false,
            insecure: false,
            app_servers: vec![],
            color: None,
            stripe_color_secondary: None,
            cluster: Default::default(),
            uri: None,
        }
    }

    #[tokio::test]
    async fn restores_last_capture_across_connections_after_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let mut server = profile();
        let cache = HostCache::at(directory.path(), &server, "topology");
        let timeout = Duration::from_millis(20);
        let capture = cache
            .refresh(timeout, async { Ok("last good") })
            .await
            .unwrap();
        assert!(capture.cache_warning.is_none());

        server.port = 8123;
        server.name = "Another profile".into();
        let reopened = HostCache::at(directory.path(), &server, "topology");
        let failure = reopened
            .refresh::<String>(timeout, std::future::pending())
            .await
            .unwrap_err();
        assert!(failure.offline);
        let cached = reopened.load::<String>().unwrap().unwrap();
        assert_eq!(cached.data, "last good");
        assert_eq!(cached.captured_at, capture.captured_at);

        let denied = reopened
            .refresh::<String>(timeout, async { anyhow::bail!("Permission denied") })
            .await
            .unwrap_err();
        assert!(!denied.offline);
        server.username = "guest".into();
        assert!(
            HostCache::at(directory.path(), &server, "topology")
                .load::<String>()
                .unwrap()
                .is_none()
        );
        server.username = "operator".into();
        server.host = "other-host".into();
        assert!(
            HostCache::at(directory.path(), &server, "topology")
                .load::<String>()
                .unwrap()
                .is_none()
        );
    }
}
