use crate::{
    host_cache::{Capture, HostCache, ReadError},
    workspace::{Connection, server_for},
};
use anyhow::Context;
use marklogic_core::{client::MarkLogicClient, config::AppConfig};
use serde::Serialize;
use std::time::Duration;

#[derive(serde::Deserialize, Serialize)]
pub struct RewriterSource {
    path: String,
    root: String,
    database: String,
    xml: String,
    message: String,
}

#[tauri::command]
pub async fn load_rewriter(
    connection: Connection,
    cached: bool,
) -> Result<Option<Capture<RewriterSource>>, ReadError> {
    let profile = server_for(&AppConfig::load()?, &connection)?;
    let cache = HostCache::new(&profile, &format!("rewriter-{}", profile.port));
    if cached {
        return Ok(cache.load()?);
    }
    let mut client = MarkLogicClient::new(profile)?.without_curl_debug();
    client.set_database(connection.database);
    cache
        .refresh(Duration::from_secs(20), async {
            let source = client
                .configuration_query(include_str!("rewriter.xqy"))
                .await
                .context("Unable to load this app server's rewriter")?;
            serde_json::from_value(source).context("Invalid rewriter response")
        })
        .await
        .map(Some)
}
