use crate::workspace::{Connection, client_for};
use marklogic_core::{
    config::AppConfig,
    topology::{self, Snapshot},
};

#[tauri::command]
pub async fn topology_snapshot(connection: Connection) -> Result<Snapshot, String> {
    let read = async {
        let client = client_for(&AppConfig::load()?, &connection)?;
        topology::discover(&client).await
    };
    tokio::time::timeout(std::time::Duration::from_secs(45), read)
        .await
        .map_err(|_| "Topology discovery timed out after 45 seconds".to_string())?
        .map_err(|error| format!("{error:#}"))
}
