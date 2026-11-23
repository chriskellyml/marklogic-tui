use crate::{
    host_cache::{self, Capture, HostCache, ReadError},
    workspace::{Connection, client_for},
};
use anyhow::Context;
use marklogic_core::{
    client::MarkLogicClient,
    config::AppConfig,
    meters::{self, RawSample},
};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[tauri::command]
pub async fn raw_meters(
    connection: Connection,
    database: String,
    cached: bool,
) -> Result<Option<Capture<Vec<RawSample>>>, ReadError> {
    let config = AppConfig::load()?;
    let profile = host_cache::profile(&config, &connection.server)?;
    let key = format!("raw-meters-{:x}", Sha256::digest(database.as_bytes()));
    let cache = HostCache::new(&profile, &key);
    if cached {
        return Ok(cache.load()?);
    }
    let client = client_for(&config, &connection)?.without_curl_debug();
    let read = async {
        let samples = meters::raw_samples(client, database).await?;
        // An empty result can occur when metering is disabled or permissions
        // change. Keep any previously captured samples in that case.
        anyhow::ensure!(
            !samples.is_empty(),
            "No readable raw meter samples were found in this database"
        );
        Ok(samples)
    };
    cache.refresh(Duration::from_secs(20), read).await.map(Some)
}

#[tauri::command]
pub async fn support_dump(
    connection: Connection,
    cached: bool,
) -> Result<Option<Capture<String>>, ReadError> {
    let profile = host_cache::profile(&AppConfig::load()?, &connection.server)?;
    let cache = HostCache::new(&profile, "support-status");
    if cached {
        return Ok(cache.load()?);
    }
    let client = MarkLogicClient::new(profile)?.without_curl_debug();
    cache
        .refresh(Duration::from_secs(20), async {
            client
                .support_status_dump()
                .await
                .context("Could not read the cluster support status from the Management API")
        })
        .await
        .map(Some)
}
