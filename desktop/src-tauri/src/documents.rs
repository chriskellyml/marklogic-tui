//! Read-only document exploration in the selected content database.
use crate::workspace::{Connection, client_for};
use marklogic_core::config::AppConfig;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CollectionMode {
    Any,
    All,
}

#[derive(Deserialize, Serialize)]
pub struct CollectionFilter {
    collections: Vec<String>,
    mode: CollectionMode,
}

#[derive(Deserialize, Serialize)]
pub struct UriCursor {
    start: String,
    exclusive: bool,
}

async fn evaluate(connection: Connection, input: Value, script: &str) -> Result<Value, String> {
    let run = async {
        let client = client_for(&AppConfig::load()?, &connection)?;
        // JSON literals keep collection names and URIs out of executable source.
        let source = format!(
            "const input = {};\n{}",
            serde_json::to_string(&input)?,
            script
        );
        let parts = client.js_query(&source).await?;
        anyhow::ensure!(parts.len() == 1, "Expected one document browser response");
        Ok::<Value, anyhow::Error>(serde_json::from_str(&parts[0])?)
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), run)
        .await
        .map_err(|_| "Document request timed out after 30 seconds".to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn browse_documents(
    connection: Connection,
    prefix: String,
    cursor: Option<UriCursor>,
    filter: CollectionFilter,
) -> Result<Value, String> {
    if filter.collections.len() > 100 {
        return Err("Choose at most 100 collections per set".into());
    }
    if !prefix.is_empty() && !prefix.ends_with('/') {
        return Err("A URI branch must end in /".into());
    }
    evaluate(
        connection,
        json!({"prefix":prefix,"cursor":cursor,"filter":filter}),
        include_str!("documents/browse.sjs"),
    )
    .await
}

#[tauri::command]
pub async fn browse_collections(
    connection: Connection,
    prefix: String,
    after: Option<String>,
) -> Result<Value, String> {
    evaluate(
        connection,
        json!({"prefix":prefix,"after":after}),
        include_str!("documents/collections.sjs"),
    )
    .await
}

#[tauri::command]
pub async fn inspect_document(connection: Connection, uri: String) -> Result<Value, String> {
    evaluate(
        connection,
        json!({"uri":uri}),
        include_str!("documents/inspect.sjs"),
    )
    .await
}
