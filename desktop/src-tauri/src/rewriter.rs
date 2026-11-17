use crate::workspace::{Connection, client_for};
use marklogic_core::config::AppConfig;
use serde_json::Value;

#[tauri::command]
pub async fn load_rewriter(connection: Connection) -> Result<Value, String> {
    let work = async {
        let client = client_for(&AppConfig::load()?, &connection)?;
        client
            .configuration_query(include_str!("rewriter.xqy"))
            .await
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), work)
        .await
        .map_err(|_| "Rewriter discovery timed out after 30 seconds".to_string())?
        .map_err(|e| format!("Unable to load this app server's rewriter: {e:#}"))
}
