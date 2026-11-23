use anyhow::{Context, Result, bail};
use marklogic_core::{
    client::{AppServerEndpoint, MarkLogicClient, ServerConfig},
    config::AppConfig,
    query_error,
    query_file::{self, QueryExecutionKind},
    tracked_folder::TrackedFolderStore,
    xquery_imports,
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, Instant},
};
use tauri::State;

#[derive(Default)]
pub struct DesktopState {
    root: Mutex<Option<PathBuf>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerProfile {
    name: String,
    endpoint: String,
    username: String,
    auth_type: marklogic_core::client::AuthType,
    color: Option<String>,
    stripe_color_secondary: Option<String>,
    app_servers: Vec<AppServerEndpoint>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerSettings {
    name: String,
    host: String,
    port: u16,
    secure: bool,
    username: String,
    auth_type: marklogic_core::client::AuthType,
    insecure: bool,
    #[serde(default)]
    color: Option<String>,
    stripe_color_secondary: Option<String>,
    app_servers: Vec<AppServerEndpoint>,
    #[serde(default, skip_serializing)]
    password: Option<String>,
}

#[tauri::command]
pub async fn server_settings(name: String) -> Result<ServerSettings, String> {
    let config = AppConfig::load().map_err(error)?;
    let s = config
        .servers
        .iter()
        .find(|s| s.name == name)
        .ok_or("Server profile no longer exists")?;
    Ok(ServerSettings {
        name: s.name.clone(),
        host: s.host.clone(),
        port: s.port,
        secure: s.secure,
        username: s.username.clone(),
        auth_type: s.auth_type,
        insecure: s.insecure,
        color: s.color.clone(),
        stripe_color_secondary: s.stripe_color_secondary.clone(),
        app_servers: s.app_servers.clone(),
        password: None,
    })
}

#[tauri::command]
pub async fn save_server_settings(
    original_name: Option<String>,
    settings: ServerSettings,
) -> Result<(), String> {
    let mut config = AppConfig::load().map_err(error)?;
    if settings.name.trim().is_empty() || settings.host.trim().is_empty() || settings.port == 0 {
        return Err("Server name, host and a valid port are required".into());
    }
    if settings.color.as_ref().is_some_and(|color| {
        color.len() != 7
            || !color.starts_with('#')
            || !color[1..].bytes().all(|b| b.is_ascii_hexdigit())
    }) {
        return Err("Environment color must be a hex code such as #ffcc00".into());
    }
    if settings
        .stripe_color_secondary
        .as_ref()
        .is_some_and(|color| {
            color.len() != 7
                || !color.starts_with('#')
                || !color[1..].bytes().all(|b| b.is_ascii_hexdigit())
        })
    {
        return Err("Second stripe color must be a hex code such as #101418".into());
    }
    if settings
        .app_servers
        .iter()
        .any(|a| a.name.trim().is_empty() || a.port == 0)
        || settings
            .app_servers
            .iter()
            .enumerate()
            .any(|(i, a)| settings.app_servers[..i].iter().any(|b| b.name == a.name))
    {
        return Err("App servers need unique names and valid ports".into());
    }
    let existing = match original_name.as_deref() {
        Some(name) => Some(
            config
                .servers
                .iter()
                .position(|s| s.name == name)
                .ok_or("Server profile no longer exists")?,
        ),
        None => None,
    };
    if config
        .servers
        .iter()
        .enumerate()
        .any(|(i, s)| s.name == settings.name && Some(i) != existing)
    {
        return Err("A server with that name already exists".into());
    }
    let password = settings.password.unwrap_or_else(|| {
        existing
            .map(|i| config.servers[i].password.clone())
            .unwrap_or_default()
    });
    let server = marklogic_core::client::ServerConfig {
        name: settings.name.clone(),
        host: settings.host,
        port: settings.port,
        secure: settings.secure,
        username: settings.username,
        password,
        auth_type: settings.auth_type,
        insecure: settings.insecure,
        app_servers: settings.app_servers,
        color: settings.color,
        stripe_color_secondary: settings.stripe_color_secondary,
        uri: None,
    };
    if let Some(i) = existing {
        if config.active_server.as_deref() == original_name.as_deref() {
            config.active_server = Some(settings.name);
        }
        config.servers[i] = server;
    } else {
        config.servers.push(server);
    }
    config.save().map_err(error)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Workspace {
    servers: Vec<ServerProfile>,
    active_server: Option<String>,
    active_app_server: Option<String>,
    active_database: Option<String>,
    config_path: PathBuf,
    query_root: PathBuf,
    tracked_folders: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    pub(crate) server: String,
    pub(crate) app_server: Option<String>,
    pub(crate) database: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryFile {
    path: String,
    language: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryFolder {
    root: PathBuf,
    files: Vec<QueryFile>,
    folders: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QueryResult {
    items: Vec<String>,
    elapsed_ms: u128,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UriResult {
    uri: String,
    collections: Vec<String>,
    permissions: Vec<String>,
    property_count: Option<usize>,
    error: Option<String>,
}

#[tauri::command]
pub async fn query_uri_metadata(
    connection: Connection,
    uris: Vec<String>,
) -> Result<Vec<UriResult>, String> {
    if uris.len() > 50 {
        return Err("At most 50 URIs can be loaded at once".into());
    }
    let config = AppConfig::load().map_err(error)?;
    let client = client_for(&config, &connection).map_err(error)?;
    let mut rows = Vec::with_capacity(uris.len());
    for value in uris {
        let uri = value.trim().to_string();
        let metadata =
            tokio::time::timeout(Duration::from_secs(10), client.get_document_metadata(&uri)).await;
        let (collections, permissions, property_count, error) = match metadata {
            Ok(Ok(meta)) => (
                meta.collections,
                meta.permissions,
                tokio::time::timeout(
                    Duration::from_secs(10),
                    client.document_property_count(&uri),
                )
                .await
                .ok()
                .and_then(Result::ok),
                None,
            ),
            Ok(Err(e)) => (vec![], vec![], None, Some(format!("{e:#}"))),
            Err(_) => (
                vec![],
                vec![],
                None,
                Some("Metadata request timed out".into()),
            ),
        };
        rows.push(UriResult {
            uri,
            collections,
            permissions,
            property_count,
            error,
        });
    }
    Ok(rows)
}

pub(crate) fn error(error: anyhow::Error) -> String {
    format!("{error:#}")
}

pub(crate) fn selected_root(state: &DesktopState) -> Result<PathBuf> {
    state
        .root
        .lock()
        .map_err(|_| anyhow::anyhow!("Workspace is unavailable"))?
        .clone()
        .context("Choose a query folder first")
}

fn query_path(root: &Path, path: &str) -> Result<PathBuf> {
    let candidate = root
        .join(path)
        .canonicalize()
        .context("Query file is unavailable")?;
    if !candidate.starts_with(root) || !candidate.is_file() {
        bail!("Query file must be inside the selected folder");
    }
    if !query_file::is_supported_query_file(&candidate) {
        bail!("Unsupported query file type");
    }
    Ok(candidate)
}

pub(crate) fn read_query(path: &Path) -> Result<String> {
    if path.metadata()?.len() > 8 * 1024 * 1024 {
        bail!("This editor supports query files up to 8 MiB");
    }
    query_file::load_query_file(path)
}

pub(crate) fn save_if_unchanged(
    path: &Path,
    contents: &str,
    expected_contents: &str,
) -> Result<()> {
    if contents.len() > 8 * 1024 * 1024 {
        bail!("This editor supports query files up to 8 MiB");
    }
    if read_query(path)? != expected_contents {
        bail!(
            "The file changed on disk. Reopen it before saving to avoid overwriting external edits."
        );
    }
    query_file::save_query_file(path, contents)
}

pub(crate) fn server_for(config: &AppConfig, connection: &Connection) -> Result<ServerConfig> {
    let mut server = config
        .servers
        .iter()
        .find(|s| s.name == connection.server)
        .cloned()
        .context("Saved server profile no longer exists. Reload the workspace.")?;
    if let Some(name) = connection.app_server.as_deref() {
        let endpoint = server
            .app_servers
            .iter()
            .find(|endpoint| endpoint.name == name)
            .context("Saved app server no longer exists. Reload the workspace.")?;
        server.port = endpoint.port;
        server.secure = endpoint.secure;
    }
    if connection.database.trim().is_empty() {
        bail!("Choose a database for this connection");
    }
    Ok(server)
}

pub(crate) fn client_for(config: &AppConfig, connection: &Connection) -> Result<MarkLogicClient> {
    let mut client = MarkLogicClient::new(server_for(config, connection)?)?;
    client.set_database(connection.database.clone());
    Ok(client)
}

#[tauri::command]
pub async fn load_workspace(state: State<'_, DesktopState>) -> Result<Workspace, String> {
    let config = AppConfig::load().map_err(error)?;
    let root = query_file::default_query_root().map_err(error)?;
    let mut selected = state.root.lock().map_err(|_| "Workspace is unavailable")?;
    let root = selected.get_or_insert(root).clone();
    let folders = TrackedFolderStore::load().map_err(error)?;
    Ok(Workspace {
        servers: config
            .servers
            .iter()
            .map(|s| ServerProfile {
                name: s.name.clone(),
                endpoint: s.base_url(),
                username: s.username.clone(),
                auth_type: s.auth_type,
                color: s.color.clone(),
                stripe_color_secondary: s.stripe_color_secondary.clone(),
                app_servers: s.app_servers.clone(),
            })
            .collect(),
        active_server: config.active_server,
        active_app_server: config.active_app_server,
        active_database: config.active_database,
        config_path: AppConfig::config_path(),
        query_root: root,
        tracked_folders: folders.folders.into_iter().map(|f| f.path).collect(),
    })
}

#[tauri::command]
pub async fn query_folder(
    state: State<'_, DesktopState>,
    root: String,
) -> Result<QueryFolder, String> {
    let root = PathBuf::from(root)
        .canonicalize()
        .map_err(|e| e.to_string())?;
    let tree = query_file::discover_query_tree(&root).map_err(error)?;
    // The desktop browser also includes supported files directly in the root.
    let folders = tree
        .folders
        .iter()
        .filter_map(|path| path.strip_prefix(&root).ok())
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let mut paths = tree.files;
    paths.extend(tree.loose_files);
    paths.sort();
    let files = paths
        .into_iter()
        .filter(|path| query_path(&root, &path.to_string_lossy()).is_ok())
        .map(|path| QueryFile {
            path: path
                .strip_prefix(&root)
                .expect("discovered inside root")
                .to_string_lossy()
                .into_owned(),
            language: match query_file::query_execution_kind(&path) {
                QueryExecutionKind::JavaScript => "javascript",
                QueryExecutionKind::XQuery => "xquery",
                _ => "text",
            },
        })
        .collect();
    *state.root.lock().map_err(|_| "Workspace is unavailable")? = Some(root.clone());
    Ok(QueryFolder {
        root,
        files,
        folders,
    })
}

#[tauri::command]
pub async fn open_query(state: State<'_, DesktopState>, path: String) -> Result<String, String> {
    let root = selected_root(&state).map_err(error)?;
    read_query(&query_path(&root, &path).map_err(error)?).map_err(error)
}

#[tauri::command]
pub async fn save_query(
    state: State<'_, DesktopState>,
    path: String,
    contents: String,
    expected_contents: String,
) -> Result<(), String> {
    let root = selected_root(&state).map_err(error)?;
    let path = query_path(&root, &path).map_err(error)?;
    save_if_unchanged(&path, &contents, &expected_contents).map_err(error)
}

#[tauri::command]
pub async fn rename_query(
    state: State<'_, DesktopState>,
    path: String,
    new_path: String,
    expected_contents: Option<String>,
) -> Result<QueryFile, String> {
    let root = selected_root(&state).map_err(error)?;
    let source = query_path(&root, &path).map_err(error)?;
    if std::fs::symlink_metadata(root.join(&path))
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("Renaming symbolic links is not supported".into());
    }
    if let Some(expected) = expected_contents {
        if read_query(&source).map_err(error)? != expected {
            return Err("The file changed on disk. Reopen it before renaming.".into());
        }
    }
    let relative = Path::new(new_path.trim());
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("Enter a relative path inside the query folder".into());
    }
    let destination = root.join(relative);
    if !query_file::is_supported_query_file(&destination) {
        return Err("Use a supported query file extension".into());
    }
    let language = match query_file::query_execution_kind(&destination) {
        QueryExecutionKind::JavaScript => "javascript",
        QueryExecutionKind::XQuery => "xquery",
        _ => "text",
    };
    let parent = destination.parent().ok_or("Invalid file name")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    if !parent
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(&root)
    {
        return Err("Query file must be inside the selected folder".into());
    }
    std::fs::hard_link(&source, &destination).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            "A file with that name already exists".to_string()
        } else {
            e.to_string()
        }
    })?;
    if let Err(e) = std::fs::remove_file(&source) {
        let _ = std::fs::remove_file(&destination);
        return Err(e.to_string());
    }
    Ok(QueryFile {
        path: relative.to_string_lossy().into_owned(),
        language,
    })
}

#[tauri::command]
pub async fn delete_query(
    state: State<'_, DesktopState>,
    path: String,
    expected_contents: Option<String>,
) -> Result<(), String> {
    let root = selected_root(&state).map_err(error)?;
    let resolved = query_path(&root, &path).map_err(error)?;
    if let Some(expected) = expected_contents {
        if read_query(&resolved).map_err(error)? != expected {
            return Err("The file changed on disk. Reopen it before deleting.".into());
        }
    }
    std::fs::remove_file(root.join(path)).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn save_scratch_query(
    state: State<'_, DesktopState>,
    name: String,
    contents: String,
    language: String,
) -> Result<QueryFile, String> {
    let root = selected_root(&state).map_err(error)?;
    if contents.len() > 8 * 1024 * 1024 {
        return Err("This editor supports query files up to 8 MiB".into());
    }
    let extension = match language.as_str() {
        "javascript" => "sjs",
        "xquery" => "xqy",
        _ => return Err("Choose JavaScript or XQuery before saving".into()),
    };
    let relative = Path::new(name.trim());
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("Enter a relative file name inside the query folder".into());
    }
    let relative = if relative.extension().is_none() {
        relative.with_extension(extension)
    } else {
        relative.to_path_buf()
    };
    let path = root.join(&relative);
    let parent = path
        .parent()
        .context("Invalid query file name")
        .map_err(error)?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let canonical_parent = parent.canonicalize().map_err(|e| e.to_string())?;
    if !canonical_parent.starts_with(&root) {
        return Err("Query file must be inside the selected folder".into());
    }
    if !query_file::is_supported_query_file(&path) {
        return Err("Use a supported .sjs or .xqy query file extension".into());
    }
    let saved_language = match query_file::query_execution_kind(&path) {
        QueryExecutionKind::JavaScript => "javascript",
        QueryExecutionKind::XQuery => "xquery",
        _ => return Err("Choose a supported JavaScript or XQuery file extension".into()),
    };
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                "A query with that name already exists".to_string()
            } else {
                e.to_string()
            }
        })?;
    file.write_all(contents.as_bytes())
        .map_err(|e| e.to_string())?;
    Ok(QueryFile {
        path: relative.to_string_lossy().into_owned(),
        language: saved_language,
    })
}

#[tauri::command]
pub async fn list_databases(connection: Connection) -> Result<Vec<String>, String> {
    let config = AppConfig::load().map_err(error)?;
    let client = client_for(&config, &connection).map_err(error)?;
    let mut databases = tokio::time::timeout(Duration::from_secs(15), client.list_databases())
        .await
        .map_err(|_| "Database discovery timed out after 15 seconds")?
        .map_err(error)?;
    databases.sort_by_key(|name| name.to_lowercase());
    Ok(databases)
}

#[tauri::command]
pub async fn execute_query(
    state: State<'_, DesktopState>,
    connection: Connection,
    source: String,
    language: String,
    path: Option<String>,
    bindings: Option<std::collections::BTreeMap<String, crate::query_variables::Binding>>,
) -> Result<QueryResult, String> {
    let started = Instant::now();
    let config = AppConfig::load().map_err(error)?;
    let client = client_for(&config, &connection).map_err(error)?;
    if source.trim().is_empty() {
        return Err("Write a query before running it".into());
    }
    let source = if language == "xquery" {
        if let Some(path) = path {
            let root = selected_root(&state).map_err(error)?;
            let path = query_path(&root, &path).map_err(error)?;
            xquery_imports::expand_xquery_imports(&source, &path).map_err(error)?
        } else {
            source
        }
    } else {
        source
    };
    let source = if language == "xquery" {
        crate::query_variables::bind(
            &source,
            &bindings.unwrap_or_default(),
            &config.active_variables,
        )?
    } else {
        source
    };
    let source =
        marklogic_core::query::substitute_active_variables(&source, &config.active_variables);
    let result = tokio::time::timeout(Duration::from_secs(120), async {
        match language.as_str() {
            "javascript" => client.js_query(&source).await,
            "xquery" => client.xquery_query(&source).await,
            _ => bail!(
                "Execution supports JavaScript and XQuery; this file can still be edited and saved"
            ),
        }
    })
    .await
    .map_err(|_| "No response after 120 seconds. The query may still be running on MarkLogic.")?;
    let items = result.map_err(|e| query_error::format_message(&format!("Query error: {e:#}")))?;
    Ok(QueryResult {
        items,
        elapsed_ms: started.elapsed().as_millis(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_uses_saved_credentials_and_selected_app_server() {
        let mut config = AppConfig::default();
        config.servers.push(marklogic_core::client::ServerConfig {
            name: "local".into(),
            host: "localhost".into(),
            username: "user".into(),
            password: "secret".into(),
            port: 8000,
            secure: false,
            auth_type: Default::default(),
            insecure: false,
            color: None,
            stripe_color_secondary: None,
            uri: None,
            app_servers: vec![AppServerEndpoint {
                port: 8443,
                secure: true,
                ..AppServerEndpoint::app_services_default()
            }],
        });
        let mut connection = Connection {
            server: "local".into(),
            app_server: Some("App-Services".into()),
            database: "Content".into(),
        };
        let client = client_for(&config, &connection).unwrap();
        assert_eq!(client.server.base_url(), "https://localhost:8443");
        assert_eq!(client.server.password, "secret");
        assert_eq!(client.database.as_deref(), Some("Content"));
        connection.app_server = Some("removed".into());
        assert!(client_for(&config, &connection).is_err());
    }

    #[test]
    fn file_access_stays_in_root_and_save_detects_external_edits() {
        let root = marklogic_core::config::test_storage_dir().join("desktop");
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let path = root.join("query.xqy");
        std::fs::write(&path, "1 + 1").unwrap();
        assert_eq!(query_path(&root, "query.xqy").unwrap(), path);
        std::fs::write(root.parent().unwrap().join("outside.xqy"), "private").unwrap();
        std::fs::write(root.join("config.toml"), "private").unwrap();
        assert!(query_path(&root, "../outside.xqy").is_err());
        assert!(query_path(&root, "config.toml").is_err());
        assert!(save_if_unchanged(&path, "new", "stale").is_err());
        assert_eq!(read_query(&path).unwrap(), "1 + 1");
        save_if_unchanged(&path, "2 + 2", "1 + 1").unwrap();
        assert_eq!(read_query(&path).unwrap(), "2 + 2");
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
