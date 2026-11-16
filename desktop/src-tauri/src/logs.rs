use crate::workspace::{Connection, client_for};
use anyhow::{Context, Result, bail};
use marklogic_core::{
    client::MarkLogicClient,
    config::AppConfig,
    logs::{
        self, LogFile, retention,
        store::{self, Inventory, Page, Query},
    },
};
use serde::Serialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use tauri::{State, ipc::Channel};
use tokio::{sync::Notify, task::JoinHandle};

#[derive(Default)]
pub struct LogState {
    next: AtomicU64,
    sessions: Mutex<BTreeMap<u64, Arc<Session>>>,
    // Both viewers write to the same log cache; serialize refreshes and downloads.
    refresh_lock: tokio::sync::Mutex<()>,
    maintenance: Arc<AtomicBool>,
}
struct Session {
    id: u64,
    client: MarkLogicClient,
    directory: String,
    root: PathBuf,
    inventory: Mutex<Inventory>,
    job: Mutex<Option<Arc<Job>>>,
    query: Mutex<Option<(u64, Arc<AtomicBool>)>>,
    index_lock: Mutex<Option<store::Reader>>,
    closed: AtomicBool,
}
struct Job {
    cancelled: AtomicBool,
    notify: Notify,
    bytes: Arc<AtomicU64>,
    progress: Mutex<Progress>,
    channel: Channel<Progress>,
}
#[derive(Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    phase: String,
    current: String,
    completed: usize,
    total: usize,
    bytes: u64,
    message: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileView {
    id: String,
    host: String,
    name: String,
    kind: String,
    port: String,
    size: u64,
    cached_bytes: u64,
    cached: bool,
    complete: bool,
    stale: bool,
    remote: bool,
    modified: String,
    days: Vec<String>,
    updated_at: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    session: u64,
    files: Vec<FileView>,
    hosts: Vec<String>,
    now: String,
    cache_root: PathBuf,
    archive_settings: retention::Settings,
    warnings: Vec<String>,
    errors: BTreeMap<String, String>,
}
fn err(error: anyhow::Error) -> String {
    format!("{error:#}")
}

#[tauri::command]
pub async fn logs_health(connection: Connection) -> Result<String, String> {
    let config = AppConfig::load().map_err(err)?;
    let client = client_for(&config, &connection).map_err(err)?;
    let value = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.logs_eval(
            "xdmp:to-json-string(fn:string(fn:current-dateTime()))",
            &json!({}),
        ),
    )
    .await
    .map_err(|_| "Connection check timed out".to_string())?
    .map_err(err)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "Invalid server time in connection check".to_string())
}
fn current(state: &LogState, id: u64) -> Result<Arc<Session>> {
    state
        .sessions
        .lock()
        .map_err(|_| anyhow::anyhow!("Log workspace unavailable"))?
        .get(&id)
        .filter(|s| !s.closed.load(Ordering::Relaxed))
        .cloned()
        .context("Log session changed; reopen Logs")
}
fn stop(session: &Session) {
    session.closed.store(true, Ordering::Relaxed);
    if let Ok(query) = session.query.lock() {
        if let Some((_, query)) = query.as_ref() {
            query.store(true, Ordering::Relaxed);
        }
    }
    if let Ok(job) = session.job.lock() {
        if let Some(job) = job.as_ref() {
            job.cancelled.store(true, Ordering::Relaxed);
            job.notify.notify_one();
        }
    }
}
fn view(session: &Session) -> Result<View> {
    let catalog = store::catalog(&session.root)?;
    let indexed_days = store::cached_days(&session.root).unwrap_or_default();
    let inventory = session
        .inventory
        .lock()
        .map_err(|_| anyhow::anyhow!("Log catalog unavailable"))?
        .clone();
    let mut known: BTreeMap<String, LogFile> = catalog
        .sources
        .iter()
        .map(|(id, s)| (id.clone(), s.snapshot.file.clone()))
        .collect();
    let remote: BTreeSet<_> = inventory
        .files
        .iter()
        .map(|f| logs::hash(&f.key()))
        .collect();
    for file in inventory.files {
        known.insert(logs::hash(&file.key()), file);
    }
    let mut hosts = BTreeSet::new();
    let mut files = Vec::new();
    for (id, file) in known {
        hosts.insert(file.host.clone());
        let source = catalog.sources.get(&id);
        let (kind, port) = logs::presentation::source(&file.name);
        files.push(FileView {
            remote: remote.contains(&id),
            days: {
                let mut days = indexed_days.get(&id).cloned().unwrap_or_default();
                if let Some(source) = source {
                    days.extend(store::source_days(source));
                }
                days.sort();
                days.dedup();
                days
            },
            id,
            host: file.host,
            name: file.name,
            kind: kind.label().into(),
            port,
            size: file.size,
            cached_bytes: source.map_or(0, |s| s.snapshot.bytes()),
            cached: source.is_some(),
            complete: source.is_some_and(|s| s.snapshot.complete()),
            stale: source.is_some_and(|s| {
                s.snapshot.file.size != file.size || s.snapshot.file.modified != file.modified
            }),
            modified: file.modified,
            updated_at: source.and_then(|s| s.snapshot.updated_at.clone()),
        });
    }
    files.sort_by(|a, b| a.name.cmp(&b.name).then(a.host.cmp(&b.host)));
    Ok(View {
        session: session.id,
        files,
        hosts: hosts.into_iter().collect(),
        now: inventory.now,
        cache_root: session.root.clone(),
        archive_settings: retention::load(&session.root)?,
        warnings: catalog.warnings,
        errors: inventory.errors,
    })
}
struct Guard {
    session: Arc<Session>,
    job: Arc<Job>,
    ticker: JoinHandle<()>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.ticker.abort();
        if let Ok(mut current) = self.session.job.lock() {
            if current.as_ref().is_some_and(|j| Arc::ptr_eq(j, &self.job)) {
                *current = None;
            }
        }
    }
}
fn begin(
    session: &Arc<Session>,
    channel: Channel<Progress>,
    phase: &str,
    total: usize,
) -> Result<Guard> {
    let mut current = session
        .job
        .lock()
        .map_err(|_| anyhow::anyhow!("Log job unavailable"))?;
    if current.is_some() {
        bail!("A log operation is already running");
    }
    let job = Arc::new(Job {
        cancelled: AtomicBool::new(false),
        notify: Notify::new(),
        bytes: Arc::new(AtomicU64::new(0)),
        progress: Mutex::new(Progress {
            phase: phase.into(),
            total,
            ..Progress::default()
        }),
        channel,
    });
    *current = Some(job.clone());
    let running = job.clone();
    let ticker = tokio::spawn(async move {
        loop {
            if let Ok(progress) = running.progress.lock() {
                let mut value = progress.clone();
                value.bytes = running.bytes.load(Ordering::Relaxed);
                if running.channel.send(value).is_err() {
                    running.cancelled.store(true, Ordering::Relaxed);
                    running.notify.notify_one();
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
    Ok(Guard {
        session: session.clone(),
        job,
        ticker,
    })
}
fn report(job: &Job, current: &str, completed: usize, total: usize) {
    if let Ok(mut progress) = job.progress.lock() {
        progress.current = current.into();
        progress.completed = completed;
        progress.total = total;
        progress.bytes = job.bytes.load(Ordering::Relaxed);
        let _ = job.channel.send(progress.clone());
    }
}
async fn request<T>(job: &Job, future: impl Future<Output = Result<T>>) -> Result<T> {
    if job.cancelled.load(Ordering::Relaxed) {
        bail!("Log operation cancelled");
    }
    tokio::select! {_ = job.notify.notified()=>bail!("Log operation cancelled"),result=future=>result}
}

#[tauri::command]
pub async fn logs_open(
    state: State<'_, LogState>,
    connection: Connection,
    directory: String,
) -> Result<View, String> {
    let id = state.next.fetch_add(1, Ordering::Relaxed) + 1;
    let config = AppConfig::load().map_err(err)?;
    let client = client_for(&config, &connection).map_err(err)?;
    let root = logs::cache_root(&client, &directory);
    let root_copy = root.clone();
    let inventory = tauri::async_runtime::spawn_blocking(move || store::load_inventory(&root_copy))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    let session = Arc::new(Session {
        id,
        client,
        directory,
        root,
        inventory: Mutex::new(inventory),
        job: Mutex::new(None),
        query: Mutex::new(None),
        index_lock: Mutex::new(None),
        closed: AtomicBool::new(false),
    });
    state
        .sessions
        .lock()
        .map_err(|_| "Log workspace unavailable")?
        .insert(id, session.clone());
    tauri::async_runtime::spawn_blocking(move || view(&session))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[tauri::command]
pub async fn logs_discover(
    state: State<'_, LogState>,
    session: u64,
    events: Channel<Progress>,
) -> Result<View, String> {
    let _refresh = state.refresh_lock.lock().await;
    let session = current(&state, session).map_err(err)?;
    let guard = begin(&session, events, "discovery", 0).map_err(err)?;
    let job = &guard.job;
    let client = session
        .client
        .clone()
        .with_download_counter(job.bytes.clone());
    let discovery = request(job, client.logs_eval(logs::DISCOVER, &json!({})))
        .await
        .map_err(err)?;
    let hosts = discovery["hosts"]
        .as_array()
        .context("Invalid cluster discovery")
        .map_err(err)?;
    let hosts: BTreeSet<String> = hosts
        .iter()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    if hosts.is_empty() {
        return Err("Cluster discovery returned no hosts".into());
    }
    let mut inventory = session
        .inventory
        .lock()
        .map_err(|_| "Log catalog unavailable")?
        .clone();
    inventory.now = discovery["now"].as_str().unwrap_or("").into();
    inventory.errors.clear();
    inventory.files.retain(|f| hosts.contains(&f.host));
    for (index, host) in hosts.iter().enumerate() {
        report(job, host, index, hosts.len());
        match request(
            job,
            logs::inventory(
                &client,
                host,
                &session.directory,
                u32::MAX,
                true,
                &BTreeSet::new(),
            ),
        )
        .await
        {
            Ok(response) => {
                let mut files: Vec<LogFile> =
                    serde_json::from_value(response["files"].clone()).map_err(|e| e.to_string())?;
                for file in &mut files {
                    file.present = true;
                }
                inventory.files.retain(|f| &f.host != host);
                inventory.files.extend(files);
            }
            Err(error) => {
                if job.cancelled.load(Ordering::Relaxed) {
                    return Err(err(error));
                }
                inventory.errors.insert(host.clone(), err(error));
            }
        }
    }
    report(job, "Discovery complete", hosts.len(), hosts.len());
    *session
        .inventory
        .lock()
        .map_err(|_| "Log catalog unavailable")? = inventory.clone();
    let root = session.root.clone();
    tauri::async_runtime::spawn_blocking(move || store::save_inventory(&root, &inventory))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    tauri::async_runtime::spawn_blocking(move || view(&session))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[tauri::command]
pub async fn logs_download(
    state: State<'_, LogState>,
    session: u64,
    files: Vec<String>,
    full: bool,
    automatic: bool,
    events: Channel<Progress>,
) -> Result<View, String> {
    let _refresh = state.refresh_lock.lock().await;
    let session = current(&state, session).map_err(err)?;
    if files.is_empty() || files.len() > 500 {
        return Err("Select between 1 and 500 remote log files".into());
    }
    let inventory = session
        .inventory
        .lock()
        .map_err(|_| "Log catalog unavailable")?
        .clone();
    let known: BTreeMap<_, _> = inventory
        .files
        .into_iter()
        .map(|f| (logs::hash(&f.key()), f))
        .collect();
    let selected: Vec<LogFile> = files
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|id| {
            known
                .get(&id)
                .cloned()
                .context("Discover logs before downloading this file")
        })
        .collect::<Result<_>>()
        .map_err(err)?;
    let guard = begin(
        &session,
        events,
        if full { "download" } else { "tail" },
        selected.len(),
    )
    .map_err(err)?;
    let job = guard.job.clone();
    let client = session
        .client
        .clone()
        .with_download_counter(job.bytes.clone())
        .with_live_log_tag(automatic);
    for (index, file) in selected.iter().enumerate() {
        report(
            &job,
            &format!("{} / {}", file.host, file.name),
            index,
            selected.len(),
        );
        let root = session.root.clone();
        let catalog = tauri::async_runtime::spawn_blocking(move || store::catalog(&root))
            .await
            .map_err(|e| e.to_string())?
            .map_err(err)?;
        let old = catalog.sources.get(&logs::hash(&file.key())).cloned();
        let request_value = if !full && old.as_ref().is_some_and(|s| s.snapshot.time.is_none()) {
            old.as_ref().unwrap().snapshot.request()
        } else {
            json!({"path":file.path,"offset":-1,"anchor_start":0,"anchor":"","full":full})
        };
        let response = request(
            &job,
            logs::collect(
                &client,
                &file.host,
                &session.directory,
                &[request_value],
                "",
            ),
        )
        .await
        .map_err(err)?;
        let updated: Vec<LogFile> =
            serde_json::from_value(response["files"].clone()).map_err(|e| e.to_string())?;
        let updated = updated
            .into_iter()
            .find(|f| f.path == file.path)
            .context("Downloaded file is no longer available")
            .map_err(err)?;
        let mut chunks: Vec<store::Chunk> =
            serde_json::from_value(response["chunks"].clone()).map_err(|e| e.to_string())?;
        if let Some(chunk) = chunks.pop() {
            if full && !chunk.full {
                return Err("Server returned a partial response for a full download".into());
            }
            let root = session.root.clone();
            let job = job.clone();
            let saved = updated.clone();
            tauri::async_runtime::spawn_blocking(move || {
                store::save_chunk(&root, saved, chunk, old.as_ref(), &job.cancelled)
            })
            .await
            .map_err(|e| e.to_string())?
            .map_err(err)?;
        } else if full || old.is_none() {
            return Err("Server returned no log content; previous cache retained".into());
        }
        let saved_inventory = {
            let mut inventory = session
                .inventory
                .lock()
                .map_err(|_| "Log catalog unavailable")?;
            inventory.files.retain(|f| f.key() != updated.key());
            inventory.files.push(updated);
            inventory.clone()
        };
        let root = session.root.clone();
        tauri::async_runtime::spawn_blocking(move || {
            store::save_inventory(&root, &saved_inventory)
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)?;
    }
    report(&job, "Complete", selected.len(), selected.len());
    tauri::async_runtime::spawn_blocking(move || view(&session))
        .await
        .map_err(|e| e.to_string())?
        .map_err(err)
}

#[tauri::command]
pub async fn logs_page(
    state: State<'_, LogState>,
    session: u64,
    query: Query,
    events: Channel<Progress>,
) -> Result<Page, String> {
    if state.maintenance.load(Ordering::Acquire) {
        return Err("Log archive is running; retry when it finishes".into());
    }
    let session = current(&state, session).map_err(err)?;
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut old = session.query.lock().map_err(|_| "Log search unavailable")?;
        if let Some((_, old)) = old.replace((query.request_id, cancel.clone())) {
            old.store(true, Ordering::Relaxed);
        }
    }
    tauri::async_runtime::spawn_blocking(move || {
        let mut reader = session
            .index_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("Log index unavailable"))?;
        if reader.is_none() {
            *reader = Some(store::Reader::open(&session.root)?);
        }
        reader
            .as_mut()
            .unwrap()
            .page(&query, cancel, |name, bytes| {
                let _ = events.send(Progress {
                    phase: "indexing".into(),
                    current: name.into(),
                    bytes,
                    ..Progress::default()
                });
            })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(err)
}

#[tauri::command]
pub fn logs_archive_settings(
    state: State<'_, LogState>,
    session: u64,
    settings: retention::Settings,
) -> Result<(), String> {
    let session = current(&state, session).map_err(err)?;
    retention::save(&session.root, &settings).map_err(err)
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveResult {
    view: View,
    report: retention::Report,
}
struct Maintenance(Arc<AtomicBool>);
impl Drop for Maintenance {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[tauri::command]
pub async fn logs_archive(
    state: State<'_, LogState>,
    session: u64,
    settings: retention::Settings,
    events: Channel<Progress>,
) -> Result<ArchiveResult, String> {
    let _refresh = state.refresh_lock.lock().await;
    let session = current(&state, session).map_err(err)?;
    retention::save(&session.root, &settings).map_err(err)?;
    let guard = begin(&session, events, "archive", 0).map_err(err)?;
    state.maintenance.store(true, Ordering::Release);
    let maintenance = Maintenance(state.maintenance.clone());
    let peers: Vec<_> = state
        .sessions
        .lock()
        .map_err(|_| "Log workspace unavailable")?
        .values()
        .filter(|s| s.root == session.root)
        .cloned()
        .collect();
    for peer in &peers {
        if let Some((_, query)) = peer
            .query
            .lock()
            .map_err(|_| "Log search unavailable")?
            .as_ref()
        {
            query.store(true, Ordering::Relaxed);
        }
    }
    let job = guard.job.clone();
    let result = tauri::async_runtime::spawn_blocking(move || -> Result<ArchiveResult> {
        let _maintenance = maintenance;
        // Close every connection to this derived index before reclaiming it.
        let mut readers = Vec::new();
        for peer in &peers {
            let mut reader = peer
                .index_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("Log index unavailable"))?;
            *reader = None;
            readers.push(reader);
        }
        let today = session
            .inventory
            .lock()
            .ok()
            .and_then(|i| chrono::DateTime::parse_from_rfc3339(&i.now).ok())
            .map(|t| chrono::Utc::now().with_timezone(t.offset()).date_naive())
            .unwrap_or_else(|| chrono::Utc::now().date_naive());
        let report = retention::run(
            &session.root,
            &settings,
            today,
            &job.cancelled,
            |name, bytes| {
                job.bytes.store(bytes, Ordering::Relaxed);
                report(&job, name, 0, 0);
            },
        )?;
        Ok(ArchiveResult {
            view: view(&session)?,
            report,
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(err);
    drop(guard);
    result
}

#[tauri::command]
pub fn logs_cancel(state: State<'_, LogState>, session: u64) -> Result<(), String> {
    let session = current(&state, session).map_err(err)?;
    if let Some(job) = session
        .job
        .lock()
        .map_err(|_| "Log job unavailable")?
        .as_ref()
    {
        job.cancelled.store(true, Ordering::Relaxed);
        job.notify.notify_one();
    }
    Ok(())
}
#[tauri::command]
pub fn logs_close(state: State<'_, LogState>, session: u64) -> Result<(), String> {
    if let Some(session) = state
        .sessions
        .lock()
        .map_err(|_| "Log workspace unavailable")?
        .remove(&session)
    {
        stop(&session);
    }
    Ok(())
}

#[tauri::command]
pub fn logs_cancel_search(
    state: State<'_, LogState>,
    session: u64,
    request_id: u64,
) -> Result<(), String> {
    let session = current(&state, session).map_err(err)?;
    if let Some((id, cancel)) = session
        .query
        .lock()
        .map_err(|_| "Log search unavailable")?
        .as_ref()
    {
        if *id == request_id {
            cancel.store(true, Ordering::Relaxed);
        }
    }
    Ok(())
}

#[tauri::command]
pub fn logs_validate_filter_rules(
    rules: Vec<logs::content_filter::Rule>,
) -> std::result::Result<(), String> {
    logs::content_filter::Rules::new(&rules)
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: u64) -> Arc<Session> {
        let config = serde_json::from_value(json!({"name":"test", "host":"localhost", "username":"user", "password":"", "port":8000, "secure":false})).unwrap();
        Arc::new(Session {
            id,
            client: MarkLogicClient::new(config).unwrap(),
            directory: String::new(),
            root: PathBuf::new(),
            inventory: Mutex::new(Inventory::default()),
            job: Mutex::new(None),
            query: Mutex::new(None),
            index_lock: Mutex::new(None),
            closed: AtomicBool::new(false),
        })
    }

    #[test]
    fn full_and_embedded_log_sessions_remain_independent() {
        let state = LogState::default();
        let full = session(1);
        let embedded = session(2);
        state
            .sessions
            .lock()
            .unwrap()
            .extend([(1, full.clone()), (2, embedded)]);
        assert_eq!(current(&state, 1).unwrap().id, 1);
        assert_eq!(current(&state, 2).unwrap().id, 2);
        stop(&full);
        state.sessions.lock().unwrap().remove(&1);
        assert!(current(&state, 1).is_err());
        assert_eq!(current(&state, 2).unwrap().id, 2);
    }
}
