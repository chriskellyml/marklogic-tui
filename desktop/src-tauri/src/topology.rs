use crate::{
    host_cache::{self, Capture, HostCache, ReadError},
    workspace::{Connection, client_for},
};
use marklogic_core::{
    config::AppConfig,
    topology::{self, Snapshot},
};
use std::time::Duration;

#[tauri::command]
pub async fn topology_snapshot(
    connection: Connection,
    cached: bool,
) -> Result<Option<Capture<Snapshot>>, ReadError> {
    let config = AppConfig::load()?;
    let profile = host_cache::profile(&config, &connection.server)?;
    let cache = HostCache::new(&profile, "topology");
    if cached {
        return Ok(cache.load()?);
    }
    let client = client_for(&config, &connection)?.without_curl_debug();
    cache
        .refresh(Duration::from_secs(15), topology::discover(&client))
        .await
        .map(Some)
}
