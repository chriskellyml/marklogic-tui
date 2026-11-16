//! Desktop access to the same environment-scoped module working copies as the TUI.
use crate::workspace::{Connection, DesktopState, client_for, selected_root};
use marklogic_core::{config::AppConfig, modules_cache};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::State;

#[derive(Clone, Deserialize)]
pub struct ModuleContext {
    pub connection: Connection,
    pub database: String,
    pub uri: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedModule {
    uri: String,
    dirty: bool,
    unsynced: bool,
}
#[derive(Serialize)]
pub struct ModuleSearch {
    uris: Vec<String>,
    total: usize,
}
#[derive(Serialize)]
pub struct ModuleContent {
    content: String,
    original: String,
}
fn error(e: anyhow::Error) -> String {
    format!("{e:#}")
}
fn environment(
    state: &DesktopState,
    connection: &Connection,
    database: &str,
) -> anyhow::Result<(marklogic_core::client::MarkLogicClient, PathBuf)> {
    anyhow::ensure!(!database.trim().is_empty(), "Choose a modules database");
    let client = client_for(&AppConfig::load()?, connection)?;
    let identity = format!(
        "{}\0{}\0{}",
        client.server.name,
        client.server.base_url(),
        client.server.username
    );
    let root = selected_root(state)?
        .join(".marklogic-tui/module-environments")
        .join(modules_cache::content_hash(&identity));
    Ok((client, root))
}
pub(crate) fn working_path(
    state: &DesktopState,
    context: &ModuleContext,
) -> anyhow::Result<PathBuf> {
    let (_, root) = environment(state, &context.connection, &context.database)?;
    let manifest = modules_cache::load_manifest(&root, &context.database)?;
    anyhow::ensure!(
        manifest.entries.contains_key(&context.uri),
        "Check out this module first"
    );
    let path =
        modules_cache::working_path_for(&root, &context.database, &context.uri).canonicalize()?;
    anyhow::ensure!(
        path.starts_with(root.canonicalize()?),
        "Module file must be inside its cache"
    );
    Ok(path)
}
#[tauri::command]
pub async fn module_cache(
    state: State<'_, DesktopState>,
    connection: Connection,
    database: String,
) -> Result<Vec<CachedModule>, String> {
    let (_, root) = environment(&state, &connection, &database).map_err(error)?;
    let manifest = modules_cache::load_manifest(&root, &database).map_err(error)?;
    Ok(modules_cache::compute_statuses(&root, &database, &manifest)
        .into_iter()
        .map(|m| CachedModule {
            uri: m.uri,
            dirty: m.dirty,
            unsynced: m.unsynced,
        })
        .collect())
}
#[tauri::command]
pub async fn search_modules(
    connection: Connection,
    database: String,
    pattern: String,
    offset: usize,
) -> Result<ModuleSearch, String> {
    let client = client_for(&AppConfig::load().map_err(error)?, &connection).map_err(error)?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.search_module_uris(&database, &pattern, offset + 1, 100),
    )
    .await
    .map_err(|_| "Module search timed out")?
    .map_err(error)?;
    Ok(ModuleSearch {
        uris: result.uris.clone(),
        total: result
            .total
            .unwrap_or(offset + result.uris.len() + usize::from(result.uris.len() == 100)),
    })
}
#[tauri::command]
pub async fn checkout_module(
    state: State<'_, DesktopState>,
    context: ModuleContext,
) -> Result<(), String> {
    if context.uri.ends_with('/') {
        return Err("Directories cannot be downloaded as modules".into());
    }
    let (client, root) =
        environment(&state, &context.connection, &context.database).map_err(error)?;
    let content = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.get_document_content_from_database(&context.uri, &context.database),
    )
    .await
    .map_err(|_| "Module download timed out")?
    .map_err(error)?;
    if content.len() > 8 * 1024 * 1024 {
        return Err("Module exceeds the 8 MiB editor limit".into());
    }
    modules_cache::checkout_module(&root, &context.database, &context.uri, &content)
        .map_err(error)?;
    Ok(())
}
#[tauri::command]
pub async fn open_module(
    state: State<'_, DesktopState>,
    context: ModuleContext,
) -> Result<ModuleContent, String> {
    let path = working_path(&state, &context).map_err(error)?;
    let (_, root) = environment(&state, &context.connection, &context.database).map_err(error)?;
    Ok(ModuleContent {
        content: crate::workspace::read_query(&path).map_err(error)?,
        original: std::fs::read_to_string(modules_cache::original_path_for(
            &root,
            &context.database,
            &context.uri,
        ))
        .map_err(|e| e.to_string())?,
    })
}
#[tauri::command]
pub async fn save_module(
    state: State<'_, DesktopState>,
    context: ModuleContext,
    content: String,
    expected: String,
) -> Result<(), String> {
    let path = working_path(&state, &context).map_err(error)?;
    crate::workspace::save_if_unchanged(&path, &content, &expected).map_err(error)
}
#[tauri::command]
pub async fn push_module(
    state: State<'_, DesktopState>,
    context: ModuleContext,
    expected: String,
) -> Result<(), String> {
    let path = working_path(&state, &context).map_err(error)?;
    let content = crate::workspace::read_query(&path).map_err(error)?;
    if content != expected {
        return Err("Working copy changed on disk; reopen before syncing".into());
    }
    let (client, root) =
        environment(&state, &context.connection, &context.database).map_err(error)?;
    let original = std::fs::read_to_string(modules_cache::original_path_for(
        &root,
        &context.database,
        &context.uri,
    ))
    .map_err(|e| e.to_string())?;
    let remote = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.get_document_content_from_database(&context.uri, &context.database),
    )
    .await
    .map_err(|_| "Remote module check timed out")?
    .map_err(error)?;
    if remote != original && remote != content {
        return Err("Remote module changed since checkout. Check out again and review the diff before syncing.".into());
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.update_document_in_modules_database(&context.uri, &content, &context.database),
    )
    .await
    .map_err(|_| "Sync timed out; check the remote module before retrying")?
    .map_err(error)?;
    modules_cache::mark_synced(&root, &context.database, &context.uri, &content).map_err(error)?;
    modules_cache::checkout_module(&root, &context.database, &context.uri, &content)
        .map_err(error)?;
    Ok(())
}

#[tauri::command]
pub async fn drop_module_edits(
    state: State<'_, DesktopState>,
    context: ModuleContext,
    expected: String,
) -> Result<ModuleContent, String> {
    let path = working_path(&state, &context).map_err(error)?;
    let (client, root) =
        environment(&state, &context.connection, &context.database).map_err(error)?;
    let remote = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        client.get_document_content_from_database(&context.uri, &context.database),
    )
    .await
    .map_err(|_| "Module download timed out")?
    .map_err(error)?;
    restore_working_copy(
        &root,
        &context.database,
        &context.uri,
        &path,
        &expected,
        &remote,
    )
    .map_err(error)?;
    Ok(ModuleContent {
        content: remote.clone(),
        original: remote,
    })
}
fn restore_working_copy(
    root: &std::path::Path,
    database: &str,
    uri: &str,
    path: &std::path::Path,
    expected: &str,
    remote: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        remote.len() <= 8 * 1024 * 1024,
        "Module exceeds the 8 MiB editor limit"
    );
    crate::workspace::save_if_unchanged(path, remote, expected)?;
    modules_cache::checkout_module(root, database, uri, remote)?;
    modules_cache::mark_synced(root, database, uri, remote)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropping_edits_checks_disk_changes_and_restores_both_copies_and_sync_status() {
        let root = marklogic_core::config::test_storage_dir().join("drop-module-edits");
        let path = modules_cache::checkout_module(&root, "Modules", "/a.xqy", "original").unwrap();
        std::fs::write(&path, "local edits").unwrap();
        assert!(
            restore_working_copy(&root, "Modules", "/a.xqy", &path, "stale", "remote").is_err()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "local edits");
        restore_working_copy(&root, "Modules", "/a.xqy", &path, "local edits", "remote").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "remote");
        assert_eq!(
            std::fs::read_to_string(modules_cache::original_path_for(&root, "Modules", "/a.xqy"))
                .unwrap(),
            "remote"
        );
        let manifest = modules_cache::load_manifest(&root, "Modules").unwrap();
        let statuses = modules_cache::compute_statuses(&root, "Modules", &manifest);
        assert!(!statuses[0].dirty && !statuses[0].unsynced);
        std::fs::remove_dir_all(root).unwrap();
    }
}
