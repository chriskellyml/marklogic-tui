use crate::workspace::{Connection, client_for};
use marklogic_core::config::AppConfig;
use serde_json::Value;

#[tauri::command]
pub async fn discover_openapi(
    connection: Connection,
    database: String,
) -> Result<Vec<String>, String> {
    let work = async {
        anyhow::ensure!(!database.trim().is_empty(), "Choose a modules database");
        let client = client_for(&AppConfig::load()?, &connection)?;
        let mut uris = Vec::new();
        let matcher = regex::Regex::new(r"(?i)(^|/)[^/]*openapi[^/]*\.(yml|yaml|json)$")?;
        let mut start = 1;
        loop {
            let page = client
                .search_module_uris(&database, "*openapi*", start, 100)
                .await?;
            let count = page.uris.len();
            uris.extend(page.uris.into_iter().filter(|uri| matcher.is_match(uri)));
            if count < 100 {
                break;
            }
            start += count;
            anyhow::ensure!(
                start < 10001,
                "Too many OpenAPI files to discover; narrow the modules database"
            );
        }
        uris.sort();
        uris.dedup();
        Ok::<_, anyhow::Error>(uris)
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), work)
        .await
        .map_err(|_| "OpenAPI discovery timed out".to_string())?
        .map_err(|e| format!("{e:#}"))
}

#[tauri::command]
pub async fn load_openapi(
    connection: Connection,
    database: String,
    uri: String,
) -> Result<Value, String> {
    let work = async {
        anyhow::ensure!(!database.trim().is_empty(), "Choose a modules database");
        let client = client_for(&AppConfig::load()?, &connection)?;
        let content = client
            .get_document_content_from_database(&uri, &database)
            .await?;
        anyhow::ensure!(
            content.len() <= 8 * 1024 * 1024,
            "OpenAPI spec exceeds 8 MiB"
        );
        let document: Value = serde_yaml::from_str(&content)?;
        anyhow::ensure!(document.is_object(), "Expected an OpenAPI object");
        Ok::<_, anyhow::Error>(document)
    };
    tokio::time::timeout(std::time::Duration::from_secs(30), work)
        .await
        .map_err(|_| "OpenAPI download timed out".to_string())?
        .map_err(|e| format!("{e:#}"))
}
