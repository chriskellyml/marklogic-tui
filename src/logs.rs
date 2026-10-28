//! Host-scoped query collectors and a persistent, locally searchable log cache.
use crate::client::MarkLogicClient;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as Days, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::mpsc, task::AbortHandle, task::JoinSet};
pub(crate) mod presentation;
use chrono::FixedOffset;
use presentation::{Kind, Options, Patterns, SearchMode, Stream};
use ratatui::layout::Rect;
use ratatui::text::Line;

#[derive(Clone, Copy, Debug)]
pub(crate) enum LogMouseTarget {
    Files {
        offset: usize,
        selected: Option<usize>,
        details: usize,
    },
    Host(usize),
    Content,
}

#[derive(Clone)]
pub(crate) struct ViewRow {
    pub line: Line<'static>,
    pub timestamp: Option<NaiveDateTime>,
    pub error: bool,
    pub start: bool,
    pub search_match: bool,
}

pub(crate) struct HostView {
    pub host: String,
    pub key: String,
    pub rows: Vec<ViewRow>,
    pub matches: Vec<usize>,
    row_sources: Vec<RowSource>,
    time_index: Vec<(NaiveDateTime, usize)>,
}

#[derive(Clone)]
struct RowSource {
    key: Arc<str>,
    record: usize,
    line: usize,
    first: Option<NaiveDateTime>,
    raw_range: Option<(usize, usize)>,
    wrapped: bool,
}

// Rendered records are kept only near the viewport. The navigation indexes
// retain lightweight metadata for the whole log, not formatted text.
struct RenderedRecord {
    key: Arc<str>,
    record: usize,
    lines: Vec<Line<'static>>,
}

struct LogActivity {
    label: &'static str,
    started: Instant,
    received: Arc<AtomicU64>,
    completed: usize,
    total: usize,
    download: bool,
    failed: usize,
    active: BTreeSet<(String, String)>,
    last: Option<(String, String, bool)>,
}

const DISCOVER: &str = include_str!("logs/discover.xqy");
const INVENTORY: &str = include_str!("logs/inventory.xqy");
const COLLECT: &str = include_str!("logs/collect.xqy");
const SCAN: &str = include_str!("logs/scan.xqy");
/// Records per scan round, and rounds per poll, bounding each response.
const SCAN_LIMIT: usize = 20_000;
const SCAN_ROUNDS: usize = 50;
const INTERVAL: Duration = Duration::from_secs(5);
/// Full responses are buffered, so limit simultaneous large host downloads.
const MAX_CONCURRENT_DOWNLOADS: usize = 2;
/// Streams collected without any opt-in. Everything else is opt-in.
const DEFAULT_STREAMS: [&str; 3] = [
    "8000_ErrorLog.txt",
    "ErrorLog.txt",
    "TaskServer_ErrorLog.txt",
];
const SUBSCRIPTIONS_FILE: &str = "subscriptions.cfg";
const CATALOG_FILE: &str = "catalog.cfg";
const TS_LEN: usize = 23; // "2026-10-14 00:02:00.419"

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct LogFile {
    pub host: String,
    pub path: String,
    pub name: String,
    pub size: u64,
    pub modified: String,
    #[serde(default)]
    pub present: bool,
}
impl LogFile {
    fn key(&self) -> String {
        format!("{}\0{}", self.host, self.path)
    }
}

/// Position of a time-addressed stream (error logs read through the
/// management API): the last record held, and how many records share its
/// timestamp, so the inclusive restart window can drop what is already cached.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct TimeCursor {
    ts: String,
    count: usize,
    /// Start of the window this generation was first fetched from.
    since: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Snapshot {
    file: LogFile,
    start: u64,
    end: u64,
    length: u64,
    anchor_start: u64,
    anchor: String,
    /// Content lives in the append-only `.log` file; only caches written by
    /// older versions carry it inside the JSON metadata.
    #[serde(default, skip_serializing)]
    text: String,
    /// Bytes of `.log` committed together with this cursor.
    #[serde(default)]
    text_bytes: u64,
    generation: u64,
    #[serde(default)]
    time: Option<TimeCursor>,
    /// Remote byte size associated with a complete on-demand download.
    #[serde(default)]
    full_size: Option<u64>,
    /// When cached content was last committed, independent of remote metadata checks.
    #[serde(default)]
    updated_at: Option<String>,
}
impl Snapshot {
    /// Cursor metadata without the (large) text.
    fn meta(&self) -> Snapshot {
        Snapshot {
            file: self.file.clone(),
            start: self.start,
            end: self.end,
            length: self.length,
            anchor_start: self.anchor_start,
            anchor: self.anchor.clone(),
            text: String::new(),
            text_bytes: self.text_bytes,
            generation: self.generation,
            time: self.time.clone(),
            full_size: self.full_size,
            updated_at: self.updated_at.clone(),
        }
    }
    fn request(&self, full: bool) -> Value {
        json!({"path":self.file.path, "offset":self.end, "anchor_start":self.anchor_start, "anchor":self.anchor, "full":full, "known_size":self.file.size, "known_modified":self.file.modified, "complete":self.end == self.length})
    }
}

#[derive(Debug, Deserialize)]
struct Chunk {
    host: String,
    path: String,
    #[serde(default)]
    start: u64,
    #[serde(default)]
    end: u64,
    #[serde(default)]
    length: u64,
    #[serde(default)]
    reset: bool,
    #[serde(default)]
    text: String,
    #[serde(default)]
    anchor_start: u64,
    #[serde(default)]
    anchor: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    full: bool,
}
#[derive(Debug, Deserialize)]
struct HostResponse {
    host: String,
    directory: String,
    inventory: bool,
    read_bytes: u64,
    cluster_bytes: u64,
    files: Vec<LogFile>,
    chunks: Vec<Chunk>,
}
#[derive(Debug, Deserialize)]
struct Discovery {
    hosts: Vec<String>,
    #[serde(default)]
    now: String,
}

/// One time-addressed fetch: everything in `name` at or after `start`.
#[derive(Clone, Debug)]
struct TimedRequest {
    host: String,
    path: String,
    name: String,
    start: String,
    skip_ts: Option<String>,
    skip_count: usize,
    /// The held generation starts later than the requested window: replace it.
    reset: bool,
}

#[derive(Debug)]
struct TimedChunk {
    request: TimedRequest,
    text: String,
    last_ts: Option<String>,
    last_count: usize,
    /// Metadata returned by every timestamp scan, including scans with no
    /// new records. This keeps a tracked stream's sidebar size current
    /// without requiring a full directory inventory on every poll.
    size: Option<u64>,
    modified: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct HostPlan {
    host: String,
    client: Option<MarkLogicClient>,
    timed: Vec<TimedRequest>,
    legacy: Vec<Value>,
}
#[derive(Clone)]
struct Endpoint {
    host: String,
    client: MarkLogicClient,
}

enum CollectorEvent {
    Endpoints(Vec<Endpoint>),
    DownloadStarted {
        host: String,
        file: String,
    },
    DownloadReport {
        host: String,
        file: String,
        result: Result<HostResponse, String>,
        bytes: u64,
    },
    Report {
        host: String,
        result: Result<HostResponse, String>,
        bytes: u64,
    },
    Inventory {
        host: String,
        result: Result<HostResponse, String>,
        bytes: u64,
        revision: u64,
        days_ago: u32,
        show_untracked: bool,
    },
    Clock(String),
    Timed {
        host: String,
        chunk: Box<TimedChunk>,
        bytes: u64,
    },
    Done {
        auto_tail: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PortFilter {
    Include,
    Exclude,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TrackingMode {
    Live,
    OnDemand,
    Partial,
    Untracked,
}

impl TrackingMode {
    pub fn marker(self) -> &'static str {
        match self {
            Self::Live => "L",
            Self::OnDemand => "D",
            Self::Partial => "P",
            Self::Untracked => "U",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Live => "Live",
            Self::OnDemand => "On demand",
            Self::Partial => "Partial",
            Self::Untracked => "Untracked",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TimeWindow {
    pub start: Option<NaiveDateTime>,
    pub end: Option<NaiveDateTime>,
}
impl TimeWindow {
    fn active(self) -> bool {
        self.start.is_some() || self.end.is_some()
    }
    fn contains(self, time: Option<NaiveDateTime>) -> bool {
        if !self.active() {
            return true;
        }
        time.is_some_and(|time| {
            self.start.is_none_or(|start| time >= start) && self.end.is_none_or(|end| time <= end)
        })
    }
}

pub(crate) struct TruncationDialog {
    pub fields: [String; 3],
    pub selected: usize,
    pub error: String,
}

pub(crate) struct LogViewer {
    pub files: Vec<LogFile>,
    pub selected: usize,
    sidebar_kind: usize,
    pub files_focus: bool,
    /// A temporary, local filter for the file sidebar. This is deliberately
    /// separate from content search so narrowing a long inventory never
    /// changes the records shown in the content pane.
    pub file_filter: String,
    pub file_filter_editing: bool,
    pub search: String,
    pub search_editing: bool,
    pub context_before: usize,
    pub context_after: usize,
    pub scroll: usize,
    host_anchor_position: Option<usize>,
    /// Shared review time, retained when the active file or host changes.
    cursor_time: Option<NaiveDateTime>,
    pub horizontal: u16,
    pub following: bool,
    pub message: String,
    pub body_bytes: u64,
    pub last_bytes: u64,
    pub read_bytes: u64,
    pub cluster_bytes: u64,
    pub all_cached: bool,
    pub combined: bool,
    pub sidebar_visible: bool,
    pub options: Options,
    pub options_open: bool,
    pub options_ports: bool,
    pub option_selected: usize,
    pub time_editing: bool,
    pub time_input: String,
    pub window: TimeWindow,
    default_truncation: bool,
    pub truncation_dialog: Option<TruncationDialog>,
    pub view_width: usize,
    pub rows: Vec<ViewRow>,
    pub host_views: Vec<HostView>,
    row_sources: Vec<Option<RowSource>>,
    rendered_records: RefCell<VecDeque<RenderedRecord>>,
    first_time: Option<NaiveDateTime>,
    show_hosts: bool,
    pub mouse_regions: Vec<(Rect, LogMouseTarget)>,
    pub environment_indicator: String,
    pub host_detail_offset: usize,
    pub host_detail_capacity: usize,
    pub host_detail_count: usize,
    pub host_pane_maximized: bool,
    pub show_line_numbers: bool,
    parsed_streams: HashMap<String, Stream>,
    combined_dirty: bool,
    server_offset: FixedOffset,
    /// Days of history collected for new streams; 1 is the current day only.
    pub scope_days: u32,
    /// Height of the content pane, so following only sticks to the bottom
    /// when the user has not scrolled away.
    pub view_height: usize,
    extra_streams: BTreeSet<String>,
    disabled_streams: BTreeSet<String>,
    modes: BTreeMap<String, TrackingMode>,
    pub show_untracked: bool,
    pub discovery_days_ago: u32,
    discovery_revision: u64,
    clock: Option<(NaiveDateTime, Instant)>,
    port_filters: BTreeMap<String, PortFilter>,
    pub include_only_marked_ports: bool,
    counter: Arc<AtomicU64>,
    pub host_bytes: BTreeMap<String, u64>,
    pub errors: BTreeMap<String, String>,
    download_results: BTreeMap<(String, String), bool>,
    pub cache_root: PathBuf,
    pub busy: bool,
    activity: Option<LogActivity>,
    /// A discovery requested while a catch-up is running. It starts as soon
    /// as the current collector emits Done, so F/r never silently do nothing.
    refresh_queued: bool,
    demand_queued: bool,
    demand_files_queued: BTreeSet<String>,
    pub lines: Vec<String>,
    pub matches: Vec<usize>,
    snapshots: HashMap<String, Snapshot>,
    endpoints: Vec<Endpoint>,
    client: Option<MarkLogicClient>,
    directory: String,
    identity: String,
    abort: Option<AbortHandle>,
    tx: mpsc::UnboundedSender<CollectorEvent>,
    rx: mpsc::UnboundedReceiver<CollectorEvent>,
    last_poll: Instant,
    opened_from: Option<crate::AppMode>,
}

impl Default for LogViewer {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            files: Vec::new(),
            selected: 0,
            sidebar_kind: 0,
            files_focus: true,
            file_filter: String::new(),
            file_filter_editing: false,
            search: String::new(),
            search_editing: false,
            context_before: 0,
            context_after: 0,
            scroll: 0,
            host_anchor_position: None,
            cursor_time: None,
            horizontal: 0,
            following: true,
            message: String::new(),
            body_bytes: 0,
            last_bytes: 0,
            read_bytes: 0,
            cluster_bytes: 0,
            all_cached: false,
            combined: false,
            sidebar_visible: true,
            options: Options::default(),
            options_open: false,
            options_ports: false,
            option_selected: 0,
            time_editing: false,
            time_input: String::new(),
            window: TimeWindow::default(),
            default_truncation: false,
            truncation_dialog: None,
            view_width: 0,
            rows: Vec::new(),
            host_views: Vec::new(),
            row_sources: Vec::new(),
            rendered_records: RefCell::new(VecDeque::new()),
            first_time: None,
            show_hosts: false,
            mouse_regions: Vec::new(),
            environment_indicator: String::new(),
            host_detail_offset: 0,
            host_detail_capacity: 0,
            host_detail_count: 0,
            host_pane_maximized: false,
            show_line_numbers: false,
            parsed_streams: HashMap::new(),
            combined_dirty: false,
            server_offset: FixedOffset::east_opt(0).unwrap(),
            scope_days: 1,
            view_height: 0,
            extra_streams: BTreeSet::new(),
            disabled_streams: BTreeSet::new(),
            modes: BTreeMap::new(),
            show_untracked: false,
            discovery_days_ago: 0,
            discovery_revision: 0,
            clock: None,
            port_filters: BTreeMap::new(),
            include_only_marked_ports: false,
            counter: Arc::new(AtomicU64::new(0)),
            host_bytes: BTreeMap::new(),
            errors: BTreeMap::new(),
            download_results: BTreeMap::new(),
            cache_root: PathBuf::new(),
            busy: false,
            activity: None,
            refresh_queued: false,
            demand_queued: false,
            demand_files_queued: BTreeSet::new(),
            lines: Vec::new(),
            matches: Vec::new(),
            snapshots: HashMap::new(),
            endpoints: Vec::new(),
            client: None,
            directory: String::new(),
            identity: String::new(),
            abort: None,
            tx,
            rx,
            last_poll: Instant::now(),
            opened_from: None,
        }
    }
}

fn hash_key(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

/// The same identity-scoped cache is used by Logs and Reports, without opening a connection.
pub(crate) fn report_cache_root(client: &MarkLogicClient, directory: &str) -> PathBuf {
    let identity = format!(
        "{}\0{}\0{}\0{}",
        client.server.name,
        client.server.base_url(),
        client.server.username,
        directory
    );
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("marklogic-tui/logs")
        .join(hash_key(&identity))
}

/// Visit committed cache content only. Streaming keeps large logs off the UI thread and
/// avoids loading non-error logs. Old inline-text snapshots remain readable.
pub(crate) fn visit_cached_error_logs(
    root: &Path,
    mut visit: impl FnMut(&str, &str),
    warnings: &mut Vec<String>,
) -> Result<usize> {
    use std::io::{BufRead, BufReader};
    if !root.is_dir() {
        bail!("No downloaded logs. Open :logs and download ErrorLog.txt first.");
    }
    let mut count = 0;
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        if path.extension().and_then(|v| v.to_str()) != Some("json") {
            continue;
        }
        let snapshot: Snapshot = match fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice(&bytes)?))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                warnings.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        // Includes rotated ErrorLog_*.txt files, excludes app-server/task-server logs.
        let name = snapshot.file.name.as_str();
        if name != "ErrorLog.txt" && !(name.starts_with("ErrorLog_") && name.ends_with(".txt")) {
            continue;
        }
        if snapshot.text_bytes == 0 && snapshot.text.is_empty() {
            continue;
        }
        let result = (|| -> Result<()> {
            if !snapshot.text.is_empty() {
                for line in snapshot.text.lines() {
                    visit(&snapshot.file.host, line);
                }
            } else {
                let file = fs::File::open(path.with_extension("log"))?;
                if file.metadata()?.len() < snapshot.text_bytes {
                    bail!("Cache is shorter than its committed size");
                }
                for line in BufReader::new(file.take(snapshot.text_bytes)).lines() {
                    visit(&snapshot.file.host, &line?);
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => count += 1,
            Err(error) => warnings.push(format!("{}: {error:#}", snapshot.file.host)),
        }
    }
    Ok(count)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).with_context(|| format!("Writing {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("Saving {}", path.display()))?;
    Ok(())
}

impl LogViewer {
    pub fn open(
        &mut self,
        client: Option<MarkLogicClient>,
        directory: &str,
        origin: crate::AppMode,
        rt: &Runtime,
    ) {
        self.opened_from = Some(origin);
        // A fresh visit starts at the current time and grows forward while the
        // live collector runs. The server clock replaces this local estimate
        // as soon as it arrives.
        self.window = TimeWindow {
            start: Some(chrono::Local::now().naive_local()),
            end: None,
        };
        self.default_truncation = true;
        // A widened window is intentionally temporary. Entering logs always
        // returns to the safe default before any local cache is displayed or
        // any server request is made.
        let reset_to_today = self.scope_days != 1;
        self.scope_days = 1;
        let Some(client) = client else {
            self.cancel();
            self.client = None;
            self.endpoints.clear();
            self.message = "Choose a server with :servers first.".into();
            return;
        };
        // Credentials never enter the cache. User identity separates accounts.
        let identity = format!(
            "{}\0{}\0{}\0{}",
            client.server.name,
            client.server.base_url(),
            client.server.username,
            directory
        );
        if identity != self.identity {
            self.cancel();
            *self = Self {
                opened_from: self.opened_from.take(),
                window: self.window,
                default_truncation: self.default_truncation,
                ..Self::default()
            };
            self.identity = identity.clone();
            self.cache_root = report_cache_root(&client, directory);
            self.directory = directory.into();
            if let Err(e) = self.load_cache() {
                self.message = format!("Cache: {e:#}");
            }
        } else if reset_to_today {
            self.ensure_visible_selection();
            self.rebuild_lines();
            self.scroll_to_bottom();
        }
        self.client = Some(client.clone());
        // Cached hosts let existing streams remain usable while discovery is
        // pending. They are not an authoritative cluster roster: new hosts and
        // hosts without cached files must also be checked on every entry.
        self.endpoints = self
            .files
            .iter()
            .map(|file| file.host.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|host| Endpoint {
                host,
                client: client.clone(),
            })
            .collect();
        // With discovery collapsed, only tracked names are returned; this
        // does not expose untracked filenames or download log contents.
        self.refresh(rt);
    }

    /// Return to the existing view without resetting its history window, scroll,
    /// filters, or in-progress dialog. Reconnect only after the server changes.
    pub fn resume(
        &mut self,
        client: Option<MarkLogicClient>,
        origin: crate::AppMode,
        rt: &Runtime,
    ) {
        let same = match (&self.client, &client) {
            (Some(old), Some(new)) => {
                old.server.name == new.server.name
                    && old.server.base_url() == new.server.base_url()
                    && old.server.username == new.server.username
            }
            _ => false,
        };
        if same {
            self.opened_from = Some(origin);
        } else {
            let directory = self.directory.clone();
            self.open(client, &directory, origin, rt);
        }
    }

    /// Stop polling the previous connection when the active profile changes.
    pub fn reconcile_client(&mut self, active: Option<&MarkLogicClient>) {
        let same = match (&self.client, active) {
            (Some(old), Some(new)) => {
                old.server.name == new.server.name
                    && old.server.base_url() == new.server.base_url()
                    && old.server.username == new.server.username
            }
            (None, _) => true,
            _ => false,
        };
        if !same {
            self.cancel();
            self.client = None;
            self.endpoints.clear();
            self.message = "Log collection paused after changing connection.".into();
        }
    }

    pub fn close(&mut self) -> crate::AppMode {
        self.opened_from.take().unwrap_or(crate::AppMode::Normal)
    }

    fn begin_activity(&mut self, label: &'static str, total: usize) -> Arc<AtomicU64> {
        let received = Arc::new(AtomicU64::new(0));
        self.activity = Some(LogActivity {
            label,
            started: Instant::now(),
            received: received.clone(),
            completed: 0,
            total,
            download: false,
            failed: 0,
            active: BTreeSet::new(),
            last: None,
        });
        self.busy = true;
        self.last_bytes = 0;
        received
    }

    pub fn activity_status(&self) -> Option<String> {
        if !self.busy {
            return None;
        }
        let activity = self.activity.as_ref()?;
        let elapsed = activity.started.elapsed();
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let spinner = frames[(elapsed.as_millis() / 100 % frames.len() as u128) as usize];
        if activity.download {
            let active = activity
                .active
                .iter()
                .map(|(host, file)| format!("{}:{file}", self.host_alias(host)))
                .collect::<Vec<_>>()
                .join(", ");
            let pending = activity
                .total
                .saturating_sub(activity.completed + activity.active.len());
            let last = activity
                .last
                .as_ref()
                .map(|(host, file, failed)| {
                    format!(
                        " · last {}:{file} {}",
                        self.host_alias(host),
                        if *failed { "failed" } else { "ok" }
                    )
                })
                .unwrap_or_default();
            return Some(format!(
                "{spinner} {} · {} ok, {} failed, {pending} queued · {}{last} · {} received · {}s · x: cancel",
                activity.label,
                activity.completed.saturating_sub(activity.failed),
                activity.failed,
                if active.is_empty() {
                    "starting"
                } else {
                    &active
                },
                format_bytes(activity.received.load(Ordering::Relaxed)),
                elapsed.as_secs()
            ));
        }
        let completed = if activity.total > 0 {
            format!(
                " · {}/{} complete",
                activity.completed.min(activity.total),
                activity.total
            )
        } else {
            String::new()
        };
        let errors = if self.errors.is_empty() {
            String::new()
        } else {
            format!(" · {} host error(s)", self.errors.len())
        };
        Some(format!(
            "{spinner} {}{completed} · {} received · {}s{errors} · x: cancel",
            activity.label,
            format_bytes(activity.received.load(Ordering::Relaxed)),
            elapsed.as_secs()
        ))
    }

    fn complete_activity_step(&mut self) {
        if let Some(activity) = &mut self.activity {
            activity.completed += 1;
        }
    }

    pub fn cancel(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.refresh_queued = false;
        self.demand_queued = false;
        self.demand_files_queued.clear();
        while let Ok(event) = self.rx.try_recv() {
            if let CollectorEvent::Report {
                host,
                result,
                bytes,
            }
            | CollectorEvent::DownloadReport {
                host,
                result,
                bytes,
                ..
            } = event
            {
                self.last_bytes += bytes;
                *self.host_bytes.entry(host).or_default() += bytes;
                if let Ok(response) = result {
                    let _ = self.apply(response);
                }
            }
        }
        self.body_bytes = self.counter.load(Ordering::Relaxed);
        self.busy = false;
        self.activity = None;
        // A fresh channel isolates late results from the cancelled collector.
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = tx;
        self.rx = rx;
    }

    fn load_cache(&mut self) -> Result<()> {
        fs::create_dir_all(&self.cache_root)?;
        self.load_subscriptions();
        if let Ok(data) = fs::read(self.cache_root.join(CATALOG_FILE))
            && let Ok(files) = serde_json::from_slice::<Vec<LogFile>>(&data)
        {
            self.files = files
                .into_iter()
                .map(|mut file| {
                    file.present = false;
                    file
                })
                .collect();
        }
        for entry in fs::read_dir(&self.cache_root)? {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let mut snapshot: Snapshot = match serde_json::from_slice(&fs::read(&path)?) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if snapshot.text.is_empty() && snapshot.text_bytes > 0 {
                // Only the committed prefix counts: a crash between appending
                // content and writing the cursor must not leak uncommitted data.
                let mut bytes = match fs::read(path.with_extension("log")) {
                    Ok(bytes) if bytes.len() as u64 >= snapshot.text_bytes => bytes,
                    _ => continue,
                };
                bytes.truncate(snapshot.text_bytes as usize);
                snapshot.text = match String::from_utf8(bytes) {
                    Ok(text) => text,
                    Err(_) => continue,
                };
            } else {
                snapshot.text_bytes = snapshot.text.len() as u64;
            }
            if snapshot.updated_at.is_none() {
                // Older cache metadata did not record this timestamp. The log
                // file's modification time is the best available approximation.
                snapshot.updated_at = fs::metadata(path.with_extension("log"))
                    .and_then(|metadata| metadata.modified())
                    .or_else(|_| fs::metadata(&path).and_then(|metadata| metadata.modified()))
                    .ok()
                    .map(|time| DateTime::<Utc>::from(time).to_rfc3339());
            }
            let mut file = snapshot.file.clone();
            file.present = false;
            if !self
                .files
                .iter()
                .any(|existing| existing.key() == file.key())
            {
                self.files.push(file);
            }
            self.snapshots.insert(snapshot.file.key(), snapshot);
        }
        self.prune_untracked_inventory();
        self.sort_files();
        self.rebuild_lines();
        self.scroll_to_bottom();
        Ok(())
    }

    fn load_subscriptions(&mut self) {
        #[derive(Deserialize)]
        struct Saved {
            #[serde(default)]
            extra: BTreeSet<String>,
            #[serde(default)]
            disabled: BTreeSet<String>,
            #[serde(default)]
            modes: BTreeMap<String, TrackingMode>,
        }
        if let Ok(bytes) = fs::read(self.cache_root.join(SUBSCRIPTIONS_FILE))
            && let Ok(saved) = serde_json::from_slice::<Saved>(&bytes)
        {
            self.extra_streams = saved.extra;
            self.disabled_streams = saved.disabled;
            self.modes = saved.modes;
        }
    }

    fn save_subscriptions(&mut self) {
        let value = json!({"extra":self.extra_streams, "disabled":self.disabled_streams, "modes": self.modes});
        let result = fs::create_dir_all(&self.cache_root)
            .map_err(anyhow::Error::from)
            .and_then(|_| {
                atomic_write(
                    &self.cache_root.join(SUBSCRIPTIONS_FILE),
                    value.to_string().as_bytes(),
                )
            });
        if let Err(e) = result {
            self.message = format!("Saving subscriptions: {e:#}");
        }
    }

    /// Collected without being asked: not opted out, and either a default
    /// stream or explicitly opted in. Names apply to every host.
    pub fn is_subscribed(&self, file: &LogFile) -> bool {
        matches!(
            self.tracking_mode(file),
            TrackingMode::Live | TrackingMode::OnDemand
        )
    }

    pub fn tracking_mode(&self, file: &LogFile) -> TrackingMode {
        self.mode_for_name(&file.name)
    }

    fn mode_for_name(&self, name: &str) -> TrackingMode {
        if let Some(mode) = self.modes.get(name) {
            return *mode;
        }
        if !self.disabled_streams.contains(name)
            && (DEFAULT_STREAMS.contains(&name) || self.extra_streams.contains(name))
        {
            TrackingMode::OnDemand
        } else {
            TrackingMode::Untracked
        }
    }

    fn tracked_names(&self) -> BTreeSet<String> {
        DEFAULT_STREAMS
            .iter()
            .map(|name| (*name).to_owned())
            .chain(self.extra_streams.iter().cloned())
            .chain(self.modes.keys().cloned())
            .filter(|name| self.mode_for_name(name) != TrackingMode::Untracked)
            .collect()
    }

    pub fn toggle_subscription(&mut self, _rt: &Runtime) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let mode = match self.tracking_mode(&file) {
            TrackingMode::Untracked => TrackingMode::OnDemand,
            TrackingMode::OnDemand => TrackingMode::Live,
            TrackingMode::Live => TrackingMode::Partial,
            TrackingMode::Partial => TrackingMode::OnDemand,
        };
        self.set_selected_mode(file, mode);
    }

    pub fn untrack_selected(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if self.tracking_mode(&file) == TrackingMode::Untracked {
            self.message = format!("{} is already untracked.", file.name);
            return;
        }
        self.set_selected_mode(file, TrackingMode::Untracked);
    }

    fn set_selected_mode(&mut self, file: LogFile, mode: TrackingMode) {
        let name = file.name.clone();
        self.modes.insert(name.clone(), mode);
        self.save_subscriptions();
        // Tracking is a priority decision as well as a collection decision:
        // keep the selected file stable while moving its stream into (or out
        // of) the leading tracked group immediately.
        self.sort_files();
        if self.combined {
            self.rebuild_lines();
        }
        self.ensure_visible_selection();
        self.message = if mode == TrackingMode::Untracked {
            format!(
                "{name}: untracked on every host; cached content kept. r shows untracked files."
            )
        } else {
            format!("{name}: {} on every host.", mode.label())
        };
        if mode == TrackingMode::Live && !is_error_log(&name) {
            self.message
                .push_str(" Live polling reads the whole remote file when it changes.");
        }
    }

    /// Start of the collection window, in the server's local time.
    fn now(&self) -> NaiveDateTime {
        match self.clock {
            Some((at, since)) => {
                at + Days::milliseconds(since.elapsed().as_millis().min(i64::MAX as u128) as i64)
            }
            None => chrono::Local::now().naive_local(),
        }
    }

    fn scope_start(&self) -> String {
        scope_start_for(self.now(), self.scope_days)
    }

    fn sort_files(&mut self) {
        let selected = self.selected_file().map(LogFile::key);
        let extra_streams = &self.extra_streams;
        let disabled_streams = &self.disabled_streams;
        let modes = &self.modes;
        let tracked = |file: &LogFile| match modes.get(&file.name) {
            Some(TrackingMode::Live | TrackingMode::OnDemand) => true,
            Some(_) => false,
            None => {
                !disabled_streams.contains(&file.name)
                    && (DEFAULT_STREAMS.contains(&file.name.as_str())
                        || extra_streams.contains(&file.name))
            }
        };
        self.files.sort_by(|left, right| {
            let left_tracked = tracked(left);
            let right_tracked = tracked(right);
            right_tracked
                .cmp(&left_tracked)
                .then_with(|| left.host.cmp(&right.host))
                .then_with(|| Self::file_port_sort_key(left).cmp(&Self::file_port_sort_key(right)))
                .then_with(|| Self::log_kind_sort_key(left).cmp(&Self::log_kind_sort_key(right)))
                .then_with(|| left.name.cmp(&right.name))
        });
        self.selected = selected
            .and_then(|key| self.files.iter().position(|f| f.key() == key))
            .unwrap_or(0);
    }

    pub fn selected_file(&self) -> Option<&LogFile> {
        self.files.get(self.selected)
    }
    pub fn cache_is_partial(&self, key: &str) -> bool {
        self.snapshots
            .get(key)
            .is_some_and(|snapshot| snapshot.start > 0)
    }

    pub fn selected_cache_is_partial(&self) -> bool {
        self.selected_file()
            .is_some_and(|file| self.cache_is_partial(&file.key()))
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        let file = self.selected_file()?;
        self.snapshots.get(&file.key())?;
        Some(
            self.cache_root
                .join(format!("{}.log", hash_key(&file.key()))),
        )
    }

    pub fn cache_age(&self, file: &LogFile, now: DateTime<Utc>) -> String {
        self.snapshots
            .get(&file.key())
            .and_then(|snapshot| snapshot.updated_at.as_deref())
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| format_cache_age(now.signed_duration_since(timestamp).num_seconds()))
            .unwrap_or_else(|| "n/a".into())
    }

    pub fn group_summaries(&self, now: DateTime<Utc>) -> BTreeMap<&str, (usize, u64, String)> {
        let mut groups: BTreeMap<&str, (BTreeSet<&str>, u64, Option<DateTime<Utc>>)> =
            BTreeMap::new();
        for file in &self.files {
            let group = groups.entry(&file.name).or_default();
            group.0.insert(&file.host);
            if file.present {
                group.1 += file.size;
            }
            if !file.present || file.size > 0 {
                let updated = self
                    .snapshots
                    .get(&file.key())
                    .and_then(|snapshot| snapshot.updated_at.as_deref())
                    .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                    .map(|time| time.with_timezone(&Utc));
                if let Some(updated) = updated {
                    group.2 = Some(group.2.map_or(updated, |previous| previous.max(updated)));
                }
            }
        }
        groups
            .into_iter()
            .map(|(name, (hosts, size, newest))| {
                let age = newest
                    .map(|time| format_cache_age(now.signed_duration_since(time).num_seconds()))
                    .unwrap_or_else(|| "n/a".into());
                (name, (hosts.len(), size, age))
            })
            .collect()
    }

    pub fn select(&mut self, index: usize) {
        let cursor = self.ensure_cursor_time();
        if !self.combined {
            self.all_cached = false;
        }
        let previous_name = self.selected_file().map(|file| file.name.clone());
        self.selected = index.min(self.files.len().saturating_sub(1));
        if let Some(file) = self.selected_file() {
            self.sidebar_kind = usize::from(Self::log_kind_sort_key(file).min(2));
        }
        if self.selected_file().map(|file| file.name.as_str()) != previous_name.as_deref() {
            self.host_detail_offset = 0;
        }
        self.horizontal = 0;
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = cursor {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
    }

    pub fn scroll_host_details(&mut self, forwards: bool) {
        let maximum = self
            .host_detail_count
            .saturating_sub(self.host_detail_capacity);
        self.host_detail_offset = if forwards {
            self.host_detail_offset.saturating_add(1).min(maximum)
        } else {
            self.host_detail_offset.saturating_sub(1)
        };
    }

    fn file_days_ago(file: &LogFile) -> Option<u32> {
        let name = file.name.strip_suffix(".txt")?;
        match name.rsplit_once('_') {
            Some((_, suffix))
                if !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()) =>
            {
                suffix.parse().ok()
            }
            _ => Some(0),
        }
    }

    fn file_port(file: &LogFile) -> Option<&str> {
        if file.name.starts_with("TaskServer_") {
            return Some("TASK");
        }
        let Some((port, _)) = file.name.split_once('_') else {
            return Some("----");
        };
        (!port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())).then_some(port)
    }

    fn file_port_sort_key(file: &LogFile) -> (u8, u16, &str) {
        match Self::file_port(file) {
            Some("----") => (0, 0, ""),
            Some("TASK") => (2, 0, ""),
            Some(port) => (1, port.parse().unwrap_or(u16::MAX), port),
            None => (3, 0, ""),
        }
    }

    fn log_kind_sort_key(file: &LogFile) -> u8 {
        let name = file.name.trim_end_matches(".txt");
        let name = name
            .rsplit_once('_')
            .filter(|(_, suffix)| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or(name, |(base, _)| base);
        if name.ends_with("ErrorLog") {
            0
        } else if name.ends_with("AccessLog") {
            1
        } else if name.ends_with("RequestLog") {
            2
        } else if name.ends_with("AuditLog") {
            3
        } else {
            4
        }
    }

    /// The concise name used in the sidebar. File paths add no information
    /// there because MarkLogic's log names encode port and stream kind.
    pub fn compact_file_label(&self, file: &LogFile) -> String {
        let name = file.name.trim_end_matches(".txt");
        let (base, rotated) = name
            .rsplit_once('_')
            .filter(|(_, suffix)| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or((name, ""), |(base, suffix)| (base, suffix));
        let suffix = if rotated.is_empty() {
            String::new()
        } else {
            format!("_{rotated}")
        };
        let compact = [
            ("_ErrorLog", "E"),
            ("_AccessLog", "A"),
            ("_RequestLog", "R"),
            ("_AuditLog", "U"),
        ]
        .into_iter()
        .find_map(|(ending, short)| {
            base.strip_suffix(ending)
                .map(|prefix| format!("{prefix}_{short}"))
        })
        .or_else(|| {
            [
                ("ErrorLog", "E"),
                ("AccessLog", "A"),
                ("RequestLog", "R"),
                ("AuditLog", "U"),
            ]
            .into_iter()
            .find_map(|(full, short)| (base == full).then_some(short.to_owned()))
        })
        .unwrap_or_else(|| base.to_owned());
        format!("{compact}{suffix}")
    }

    pub fn host_alias(&self, host: &str) -> String {
        self.host_aliases()
            .get(host)
            .cloned()
            .unwrap_or_else(|| "h1".into())
    }

    pub fn host_aliases(&self) -> BTreeMap<String, String> {
        self.known_hosts()
            .into_iter()
            .enumerate()
            .map(|(index, host)| (host.to_owned(), format!("h{}", index + 1)))
            .collect()
    }

    pub fn host_legend(&self) -> String {
        self.host_legend_lines().join("  ·  ")
    }

    pub fn host_legend_lines(&self) -> Vec<String> {
        self.known_hosts()
            .into_iter()
            .enumerate()
            .map(|(index, host)| {
                let result = self.selected_file().and_then(|file| {
                    self.download_results
                        .get(&(host.to_owned(), file.name.clone()))
                });
                let marker = match result {
                    Some(true) => " [ok]",
                    Some(false) => " [failed]",
                    None => "",
                };
                format!("h{}{marker}={host}", index + 1)
            })
            .collect()
    }

    /// Every known host, with its copy of the selected filename if listed.
    pub fn selected_host_details(&self) -> Vec<(String, &str, Option<&LogFile>, Option<bool>)> {
        let aliases = self.host_aliases();
        let Some(selected) = self.selected_file() else {
            return Vec::new();
        };
        self.known_hosts()
            .into_iter()
            .map(|host| {
                let file = self
                    .files
                    .iter()
                    .find(|file| file.host == host && file.name == selected.name);
                let result = self
                    .download_results
                    .get(&(host.to_owned(), selected.name.clone()))
                    .copied();
                (aliases[host].clone(), host, file, result)
            })
            .collect()
    }

    fn known_hosts(&self) -> BTreeSet<&str> {
        self.files
            .iter()
            .map(|file| file.host.as_str())
            .chain(self.endpoints.iter().map(|endpoint| endpoint.host.as_str()))
            .collect()
    }

    fn matches_file_filter(
        &self,
        file: &LogFile,
        aliases: Option<&BTreeMap<String, String>>,
    ) -> bool {
        let needle = self.file_filter.trim().to_lowercase();
        needle.is_empty()
            || file.host.to_lowercase().contains(&needle)
            || file.name.to_lowercase().contains(&needle)
            || self
                .compact_file_label(file)
                .to_lowercase()
                .contains(&needle)
            || aliases
                .and_then(|aliases| aliases.get(&file.host))
                .cloned()
                .unwrap_or_else(|| self.host_alias(&file.host))
                .contains(&needle)
    }

    fn is_file_visible(&self, file: &LogFile) -> bool {
        self.is_file_visible_with_aliases(file, None)
    }

    fn is_file_visible_with_aliases(
        &self,
        file: &LogFile,
        aliases: Option<&BTreeMap<String, String>>,
    ) -> bool {
        if !self.is_file_available(file) {
            return false;
        }
        // The sidebar is an inventory for the chosen window. Tracking a
        // stream means it is collected, not that every historical rotation
        // should appear in a default "today" list.
        let port_filter = Self::file_port(file).and_then(|port| self.port_filters.get(port));
        if matches!(port_filter, Some(PortFilter::Exclude)) {
            return false;
        }
        if !self.matches_file_filter(file, aliases) {
            return false;
        }
        if self.include_only_marked_ports && !matches!(port_filter, Some(PortFilter::Include)) {
            return false;
        }
        true
    }

    fn is_file_available(&self, file: &LogFile) -> bool {
        let tracked = self.tracking_mode(file) != TrackingMode::Untracked;
        let visible_by_day = Self::file_days_ago(file) == Some(self.discovery_days_ago);
        visible_by_day && (tracked || (self.show_untracked && file.present))
    }

    fn is_sidebar_file_available(&self, file: &LogFile) -> bool {
        let tracked = self.tracking_mode(file) != TrackingMode::Untracked;
        Self::file_days_ago(file) == Some(self.discovery_days_ago)
            && (tracked || file.present || self.snapshots.contains_key(&file.key()))
    }

    fn is_sidebar_file_visible(&self, file: &LogFile) -> bool {
        if !self.is_sidebar_file_available(file) {
            return false;
        }
        let port_filter = Self::file_port(file).and_then(|port| self.port_filters.get(port));
        !matches!(port_filter, Some(PortFilter::Exclude))
            && self.matches_file_filter(file, None)
            && (!self.include_only_marked_ports || matches!(port_filter, Some(PortFilter::Include)))
    }

    pub fn discovery_day(&self) -> NaiveDate {
        self.now().date() - Days::days(i64::from(self.discovery_days_ago))
    }

    pub fn discovery_day_label(&self) -> String {
        let day = self.discovery_day();
        match self.discovery_days_ago {
            0 => format!("today {day}"),
            1 => format!("yesterday {day}"),
            n => format!("{n} days ago {day}"),
        }
    }

    pub fn set_discovery_days_ago(&mut self, days: u32, rt: &Runtime) {
        let days = days.min(36_500);
        if self.discovery_days_ago == days {
            return;
        }
        self.discovery_days_ago = days;
        self.discovery_revision = self.discovery_revision.wrapping_add(1);
        self.ensure_visible_selection();
        self.refresh(rt);
    }

    fn prune_untracked_inventory(&mut self) {
        let selected = self.selected_file().map(LogFile::key);
        let tracked = self.tracked_names();
        let snapshots = &self.snapshots;
        self.files
            .retain(|file| tracked.contains(&file.name) || snapshots.contains_key(&file.key()));
        self.selected = selected
            .and_then(|key| self.files.iter().position(|file| file.key() == key))
            .unwrap_or(0);
    }

    pub fn visible_file_indices(&self) -> Vec<usize> {
        let aliases = (!self.file_filter.trim().is_empty()).then(|| self.host_aliases());
        let mut names = BTreeSet::new();
        self.files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                (self.is_file_visible_with_aliases(file, aliases.as_ref())
                    && names.insert(file.name.clone()))
                .then_some(index)
            })
            .collect()
    }

    pub fn available_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| self.is_file_available(file))
            .map(|file| &file.name)
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub fn available_port_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| self.is_sidebar_file_available(file))
            .filter(|file| Self::log_kind_sort_key(file) <= 2)
            .filter_map(Self::file_port)
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub fn selected_visible_index(&self) -> Option<usize> {
        let selected = self.selected_file()?;
        self.visible_port_rows()
            .iter()
            .position(|(_, cells)| {
                cells.iter().flatten().any(|index| {
                    self.files
                        .get(*index)
                        .is_some_and(|file| file.name == selected.name)
                })
            })
            .filter(|_| self.is_sidebar_file_visible(selected))
    }

    /// A sidebar row represents a port. Each cell points to the visible file
    /// group for that port and log kind (error, access, request).
    pub fn visible_port_rows(&self) -> Vec<(String, [Option<usize>; 3])> {
        let mut rows: BTreeMap<String, [Option<usize>; 3]> = BTreeMap::new();
        for (index, file) in self.files.iter().enumerate() {
            if !self.is_sidebar_file_visible(file) {
                continue;
            }
            let Some(port) = Self::file_port(file) else {
                continue;
            };
            let kind = usize::from(Self::log_kind_sort_key(file));
            if kind > 2 {
                continue;
            }
            rows.entry(port.to_owned()).or_default()[kind] = Some(index);
        }
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_by(|(left, _), (right, _)| {
            Self::port_label_sort_key(left).cmp(&Self::port_label_sort_key(right))
        });
        rows
    }

    pub fn selected_kind_column(&self) -> usize {
        self.sidebar_kind.min(2)
    }

    fn port_label_sort_key(port: &str) -> (u8, u16, &str) {
        match port {
            "----" => (0, 0, ""),
            "TASK" => (1, 0, ""),
            _ => (2, port.parse().unwrap_or(u16::MAX), port),
        }
    }

    fn select_port_row(&mut self, index: usize, preferred_kind: usize) {
        let Some((_, cells)) = self.visible_port_rows().get(index).cloned() else {
            return;
        };
        let kind = if cells.get(preferred_kind).is_some_and(Option::is_some) {
            preferred_kind
        } else {
            cells
                .iter()
                .position(Option::is_some)
                .unwrap_or(preferred_kind)
        };
        self.sidebar_kind = kind.min(2);
        if let Some(file_index) = cells.get(kind).copied().flatten() {
            self.select(file_index);
        }
    }

    pub fn select_port_column(&mut self, forwards: bool) {
        let rows = self.visible_port_rows();
        let Some(row_index) = self.selected_visible_index() else {
            return;
        };
        let Some((_, cells)) = rows.get(row_index) else {
            return;
        };
        let direction: isize = if forwards { 1 } else { -1 };
        for step in 1..=3 {
            let column = (self.sidebar_kind as isize + direction * step).rem_euclid(3) as usize;
            if let Some(file_index) = cells[column] {
                self.sidebar_kind = column;
                self.select(file_index);
                return;
            }
        }
    }

    fn ensure_visible_selection(&mut self) {
        if self.selected_visible_index().is_none()
            && let Some((_, cells)) = self.visible_port_rows().first()
            && let Some(index) = cells.iter().flatten().next().copied()
        {
            self.select(index);
        }
    }

    pub fn select_visible(&mut self, index: usize) {
        self.select_port_row(index, self.sidebar_kind);
    }

    pub fn select_visible_next(&mut self) {
        let next = self
            .selected_visible_index()
            .map(|index| index + 1)
            .unwrap_or(0);
        self.select_visible(next);
    }

    pub fn select_visible_previous(&mut self) {
        let previous = self.selected_visible_index().unwrap_or(0).saturating_sub(1);
        self.select_visible(previous);
    }

    pub fn select_visible_first(&mut self) {
        self.select_visible(0);
    }

    pub fn select_visible_last(&mut self) {
        let last = self.visible_port_rows().len().saturating_sub(1);
        self.select_visible(last);
    }

    pub fn clear_file_filter(&mut self) {
        self.file_filter.clear();
        self.ensure_visible_selection();
    }

    pub fn edit_file_filter(&mut self, character: Option<char>, backspace: bool) {
        if backspace {
            self.file_filter.pop();
        } else if let Some(character) = character {
            self.file_filter.push(character);
        }
        self.ensure_visible_selection();
    }

    fn cycle_port(&mut self, port: &str) {
        let next = match self.port_filters.get(port) {
            None => Some(PortFilter::Include),
            Some(PortFilter::Include) => Some(PortFilter::Exclude),
            Some(PortFilter::Exclude) => None,
        };
        if let Some(filter) = next {
            self.port_filters.insert(port.to_string(), filter);
        } else {
            self.port_filters.remove(port);
        }
        self.include_only_marked_ports = self
            .port_filters
            .values()
            .any(|filter| *filter == PortFilter::Include);
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = match next {
            Some(PortFilter::Include) => format!("Showing only ports marked +, including {port}."),
            Some(PortFilter::Exclude) => format!("Port {port}: excluded."),
            None => format!("Port {port}: no port filter."),
        };
    }

    pub fn toggle_include_only_marked_ports(&mut self) {
        if !self.include_only_marked_ports
            && !self
                .port_filters
                .values()
                .any(|filter| *filter == PortFilter::Include)
        {
            self.message = "Mark at least one port + before showing only included ports.".into();
            return;
        }
        self.include_only_marked_ports = !self.include_only_marked_ports;
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = if self.include_only_marked_ports {
            "Showing only ports marked +.".into()
        } else {
            "Showing all ports except those marked -.".into()
        };
    }

    pub fn clear_port_filters(&mut self) {
        self.port_filters.clear();
        self.include_only_marked_ports = false;
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = "Cleared port filters.".into();
    }

    /// Cycle how many days new streams reach back: today, 2, 7, 30.
    pub fn cycle_scope_days(&mut self, _rt: &Runtime) {
        self.scope_days = match self.scope_days {
            1 => 2,
            2 => 7,
            7 => 30,
            _ => 1,
        };
        self.save_subscriptions();
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = format!("Showing {} of the cached history.", self.scope_label());
    }

    pub fn scope_label(&self) -> String {
        match self.scope_days {
            1 => "today".into(),
            n => format!("last {n} days"),
        }
    }

    pub fn port_filter_summary(&self) -> String {
        let mut marks = self
            .port_filters
            .iter()
            .map(|(port, filter)| match filter {
                PortFilter::Include => format!("+{port}"),
                PortFilter::Exclude => format!("-{port}"),
            })
            .collect::<Vec<_>>();
        marks.sort();
        if marks.is_empty() {
            "all ports".into()
        } else {
            format!(
                "{} ({})",
                marks.join(", "),
                if self.include_only_marked_ports {
                    "only +"
                } else {
                    "all except -"
                }
            )
        }
    }

    pub fn port_filter_marker(&self, file: &LogFile) -> &'static str {
        match Self::file_port(file).and_then(|port| self.port_filters.get(port)) {
            Some(PortFilter::Include) => "+",
            Some(PortFilter::Exclude) => "-",
            None => " ",
        }
    }
    fn rebuild_lines(&mut self) {
        if self.combined {
            self.rebuild_combined();
            return;
        }
        self.lines.clear();
        self.rows.clear();
        self.row_sources.clear();
        self.rendered_records.borrow_mut().clear();
        self.host_views.clear();
        if self.all_cached {
            let files: Vec<_> = self
                .files
                .iter()
                .filter(|file| self.is_file_visible(file))
                .cloned()
                .collect();
            for file in files {
                let key = file.key();
                let Some(snapshot) = self.snapshots.get(&key) else {
                    continue;
                };
                let key: Arc<str> = Arc::from(key.as_str());
                let mut offset = 0;
                let mut previous_time = None;
                for line in snapshot.text.split_inclusive('\n') {
                    let text = line.strip_suffix('\n').unwrap_or(line);
                    let text = text.strip_suffix('\r').unwrap_or(text);
                    // Keep timestamps available even before a truncation
                    // window is active. `t`/`T` use the current row as their
                    // bound, and the all-cached raw view must support that
                    // for ErrorLog, AccessLog, and RequestLog alike.
                    let time = presentation::line_time(&snapshot.file, text, self.server_offset)
                        .or(previous_time);
                    previous_time = time;
                    if !self.window.contains(time) {
                        offset += line.len();
                        continue;
                    }
                    self.rows.push(ViewRow {
                        line: Line::default(),
                        timestamp: time,
                        error: false,
                        start: true,
                        search_match: false,
                    });
                    self.lines.push(String::new());
                    self.row_sources.push(Some(RowSource {
                        key: key.clone(),
                        record: 0,
                        line: 0,
                        first: None,
                        raw_range: Some((offset, offset + text.len())),
                        wrapped: false,
                    }));
                    offset += line.len();
                }
            }
        } else if let Some(name) = self.selected_file().map(|file| file.name.clone()) {
            let mut files: Vec<_> = self
                .files
                .iter()
                .filter(|file| file.name == name)
                .cloned()
                .collect();
            files.sort_by(|left, right| left.host.cmp(&right.host));
            for file in files {
                let key = file.key();
                let Some(snapshot) = self.snapshots.get(&key) else {
                    continue;
                };
                if snapshot.text.trim().is_empty() || (file.present && file.size == 0) {
                    continue;
                }
                self.parsed_streams.entry(key.clone()).or_default().update(
                    &snapshot.file,
                    &snapshot.text,
                    false,
                    self.server_offset,
                );
                let Some(stream) = self.parsed_streams.get(&key) else {
                    continue;
                };
                let mut rows = Vec::new();
                let mut row_sources = Vec::new();
                let shared_key: Arc<str> = Arc::from(key.as_str());
                let range = stream.window_range(self.window.start, self.window.end);
                let first = stream.records[range.clone()]
                    .iter()
                    .filter(|record| self.window.contains(record.timestamp))
                    .find_map(|record| record.timestamp);
                for (record_index, record) in stream
                    .records
                    .iter()
                    .enumerate()
                    .skip(range.start)
                    .take(range.len())
                {
                    if !self.window.contains(record.timestamp) {
                        continue;
                    }
                    for line in 0..record.display_line_count(&self.options) {
                        rows.push(ViewRow {
                            line: Line::default(),
                            timestamp: record.timestamp,
                            error: record.is_error(),
                            start: line == 0,
                            search_match: false,
                        });
                        row_sources.push(RowSource {
                            key: shared_key.clone(),
                            record: record_index,
                            line,
                            first,
                            raw_range: None,
                            wrapped: false,
                        });
                    }
                }
                if !rows.is_empty() {
                    self.host_views.push(HostView {
                        host: file.host,
                        key,
                        rows,
                        matches: Vec::new(),
                        row_sources,
                        time_index: Vec::new(),
                    });
                }
            }
            let current = self.selected_file().map(LogFile::key);
            let lead = self
                .host_views
                .iter()
                .position(|view| Some(&view.key) == current.as_ref())
                .unwrap_or(0);
            if let Some(view) = self.host_views.get(lead) {
                if let Some(index) = self.files.iter().position(|file| file.key() == view.key) {
                    self.selected = index;
                }
                self.rows = view.rows.clone();
                self.row_sources = view.row_sources.iter().cloned().map(Some).collect();
                self.lines = vec![String::new(); self.rows.len()];
            }
        }
        self.filter();
    }
    fn materialize(&self, source: &RowSource, combined: bool) -> Line<'static> {
        if let Some((start, end)) = source.raw_range {
            return self
                .snapshots
                .get(source.key.as_ref())
                .and_then(|snapshot| {
                    snapshot.text.get(start..end).map(|text| {
                        let color = if text.contains("Error:")
                            || text.contains("Fatal:")
                            || text.contains("Alert:")
                        {
                            ratatui::style::Color::Red
                        } else if text.contains("Warning:") {
                            ratatui::style::Color::Yellow
                        } else {
                            ratatui::style::Color::White
                        };
                        Line::styled(
                            format!("[{} / {}] {text}", snapshot.file.host, snapshot.file.name),
                            ratatui::style::Style::default().fg(color),
                        )
                    })
                })
                .unwrap_or_default();
        }
        let mut cache = self.rendered_records.borrow_mut();
        let hit = cache
            .iter()
            .position(|entry| entry.key == source.key && entry.record == source.record);
        let entry = if let Some(index) = hit {
            cache.remove(index).expect("cached record")
        } else {
            let Some(stream) = self.parsed_streams.get(source.key.as_ref()) else {
                return Line::default();
            };
            let Some(record) = stream.records.get(source.record) else {
                return Line::default();
            };
            let first = if combined {
                self.first_time
            } else {
                source.first
            };
            let mut lines = record.render(&self.options, first, combined && self.show_hosts);
            if combined && self.options.search_mode != SearchMode::Off {
                let patterns = Patterns::new(&self.search);
                lines = lines
                    .into_iter()
                    .map(|line| patterns.highlight(line))
                    .collect();
            }
            if source.wrapped {
                lines = lines
                    .into_iter()
                    .flat_map(|line| presentation::wrap_line(line, self.view_width.max(1)))
                    .collect();
            }
            RenderedRecord {
                key: source.key.clone(),
                record: source.record,
                lines,
            }
        };
        let line = entry.lines.get(source.line).cloned().unwrap_or_default();
        cache.push_back(entry);
        let limit = self
            .view_height
            .max(1)
            .saturating_mul(3)
            .saturating_mul(self.host_views.len().max(1))
            .max(64);
        while cache.len() > limit {
            cache.pop_front();
        }
        line
    }

    pub(crate) fn display_row(&self, index: usize) -> Line<'static> {
        if let Some(Some(source)) = self.row_sources.get(index) {
            self.materialize(source, self.combined)
        } else if let Some(row) = self.rows.get(index) {
            row.line.clone()
        } else {
            Line::from(self.lines.get(index).cloned().unwrap_or_default())
        }
    }

    pub(crate) fn display_host_row(&self, view: &HostView, index: usize) -> Line<'static> {
        view.row_sources
            .get(index)
            .map(|source| self.materialize(source, false))
            .unwrap_or_else(|| {
                view.rows
                    .get(index)
                    .map(|row| row.line.clone())
                    .unwrap_or_default()
            })
    }

    pub fn toggle_all_cached(&mut self) {
        let cursor = self.ensure_cursor_time();
        self.all_cached = !self.all_cached;
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = cursor {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
    }
    pub fn filter(&mut self) {
        self.rendered_records.borrow_mut().clear();
        if self.combined {
            self.rebuild_combined();
            if let Some(time) = self.cursor_time {
                self.seek_nearest(time);
            }
            return;
        }
        let needle = self.search.to_lowercase();
        let leading_key = self.selected_file().map(LogFile::key);
        for view in &mut self.host_views {
            let stream = self.parsed_streams.get(&view.key);
            let mut visible = vec![false; view.rows.len()];
            let first = view.row_sources.first().and_then(|source| source.first);
            let mut last_record = None;
            let mut rendered = Vec::new();
            let mut raw_match = false;
            for (index, source) in view.row_sources.iter().enumerate() {
                let matched = if needle.is_empty() {
                    true
                } else if let Some(record) =
                    stream.and_then(|stream| stream.records.get(source.record))
                {
                    if last_record != Some(source.record) {
                        raw_match = record.raw.to_lowercase().contains(&needle);
                        rendered = if raw_match {
                            Vec::new()
                        } else {
                            record.render(&self.options, first, false)
                        };
                        last_record = Some(source.record);
                    }
                    raw_match
                        || rendered
                            .get(source.line)
                            .is_some_and(|line| line.to_string().to_lowercase().contains(&needle))
                } else {
                    false
                };
                if matched {
                    let start = index.saturating_sub(self.context_before);
                    let end = index
                        .saturating_add(self.context_after)
                        .min(view.rows.len().saturating_sub(1));
                    visible[start..=end].fill(true);
                }
            }
            view.matches = visible
                .into_iter()
                .enumerate()
                .filter_map(|(index, show)| show.then_some(index))
                .collect();
            view.time_index = view
                .matches
                .iter()
                .enumerate()
                .filter_map(|(position, &row)| {
                    view.rows[row].timestamp.map(|time| (time, position))
                })
                .collect();
            if !view.time_index.windows(2).all(|pair| pair[0] <= pair[1]) {
                view.time_index.sort_unstable();
            }
        }
        if let Some(view) = self
            .host_views
            .iter()
            .find(|view| Some(&view.key) == leading_key.as_ref())
        {
            self.matches = view.matches.clone();
            self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
            if let Some(time) = self.cursor_time {
                self.seek_nearest(time);
            }
            return;
        }
        if self.all_cached && !self.row_sources.is_empty() {
            self.matches = (0..self.row_sources.len())
                .filter(|&i| {
                    needle.is_empty()
                        || self
                            .display_row(i)
                            .to_string()
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect();
            if !needle.is_empty() {
                self.expand_context();
            }
            self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
            if let Some(time) = self.cursor_time {
                self.seek_nearest(time);
            }
            return;
        }
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| needle.is_empty() || line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        if !needle.is_empty() {
            self.expand_context();
        }
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
        if let Some(time) = self.cursor_time {
            self.seek_nearest(time);
        }
    }

    fn expand_context(&mut self) {
        if self.context_before == 0 && self.context_after == 0 {
            return;
        }
        let mut visible = vec![false; self.lines.len()];
        for &index in &self.matches {
            let start = index.saturating_sub(self.context_before);
            let end = index
                .saturating_add(self.context_after)
                .min(self.lines.len().saturating_sub(1));
            if start <= end {
                visible[start..=end].fill(true);
            }
        }
        self.matches = visible
            .into_iter()
            .enumerate()
            .filter_map(|(index, show)| show.then_some(index))
            .collect();
    }

    pub fn cycle_context(&mut self, before: bool) {
        let value = if before {
            &mut self.context_before
        } else {
            &mut self.context_after
        };
        *value = match *value {
            0 => 1,
            1 => 3,
            3 => 5,
            5 => 10,
            _ => 0,
        };
        self.filter();
    }

    /// Put the newest visible line at the bottom of the content pane.  Before
    /// the first render the pane height is unknown; using one row still gives
    /// the renderer a tail position that it can clamp to its actual height.
    pub fn scroll_to_bottom(&mut self) {
        self.host_anchor_position = None;
        self.scroll = self.matches.len().saturating_sub(self.view_height.max(1));
        if self.cursor_time.is_none() {
            self.cursor_time = self
                .matches
                .last()
                .and_then(|&row| self.rows.get(row))
                .and_then(|row| row.timestamp);
        }
    }

    pub fn leading_host_index(&self) -> Option<usize> {
        let key = self.selected_file()?.key();
        self.host_views.iter().position(|view| view.key == key)
    }

    pub fn anchor_position(&self) -> Option<usize> {
        let last = self.matches.len().checked_sub(1)?;
        if let Some(position) = self.host_anchor_position.map(|p| p.min(last)) {
            let row_time = self
                .matches
                .get(position)
                .and_then(|&row| self.rows.get(row))
                .and_then(|row| row.timestamp);
            if self.cursor_time.is_none() || row_time == self.cursor_time {
                return Some(position);
            }
        }
        if let Some(target) = self.cursor_time
            && let Some((position, _)) = self
                .matches
                .iter()
                .enumerate()
                .filter_map(|(position, &row)| {
                    self.rows
                        .get(row)
                        .and_then(|row| row.timestamp)
                        .map(|time| (position, (time - target).num_milliseconds().unsigned_abs()))
                })
                .min_by_key(|(_, distance)| *distance)
        {
            return Some(position);
        }
        Some(
            self.host_anchor_position
                .unwrap_or_else(|| {
                    if !self.combined && !self.all_cached && self.host_views.len() > 1 {
                        self.scroll.saturating_add(self.view_height.max(1) / 2)
                    } else {
                        self.scroll
                    }
                })
                .min(last),
        )
    }

    pub fn anchor_timestamp(&self) -> Option<NaiveDateTime> {
        self.cursor_time.or_else(|| {
            let position = self.anchor_position()?;
            self.rows.get(*self.matches.get(position)?)?.timestamp
        })
    }

    fn ensure_cursor_time(&mut self) -> Option<NaiveDateTime> {
        if self.cursor_time.is_none() {
            self.cursor_time = self.anchor_timestamp();
        }
        self.cursor_time
    }

    pub fn host_anchor(&self, view: &HostView) -> Option<usize> {
        if view.matches.is_empty() {
            return None;
        }
        if let Some(time) = self.anchor_timestamp() {
            let index = view
                .time_index
                .partition_point(|(candidate, _)| *candidate < time);
            if let Some((_, position)) = [index.checked_sub(1), Some(index)]
                .into_iter()
                .flatten()
                .filter_map(|index| view.time_index.get(index))
                .min_by_key(|(candidate, _)| (*candidate - time).num_milliseconds().unsigned_abs())
            {
                return Some(*position);
            }
        }
        Some(
            self.anchor_position()
                .unwrap_or(0)
                .min(view.matches.len() - 1),
        )
    }

    fn select_leading_host(&mut self, next: usize) {
        if self.leading_host_index() == Some(next) {
            return;
        }
        self.ensure_cursor_time();
        let target = self.host_anchor(&self.host_views[next]);
        let key = self.host_views[next].key.clone();
        if let Some(index) = self.files.iter().position(|file| file.key() == key) {
            self.selected = index;
            self.rebuild_lines();
            self.host_anchor_position = target;
            if let Some(position) = target {
                self.scroll = position.saturating_sub(self.view_height.max(1) / 2);
            }
        }
    }

    pub fn focus_host(&mut self, index: usize) {
        if index < self.host_views.len() {
            self.select_leading_host(index);
            self.files_focus = false;
        }
    }

    pub fn toggle_host_pane_maximized(&mut self) {
        if !self.combined && !self.all_cached && self.host_views.len() > 1 {
            self.host_pane_maximized = !self.host_pane_maximized;
        }
    }

    pub fn reset_view(&mut self) {
        self.file_filter.clear();
        self.search.clear();
        self.file_filter_editing = false;
        self.search_editing = false;
        self.time_editing = false;
        self.time_input.clear();
        self.options_open = false;
        self.combined = false;
        self.all_cached = false;
        self.following = true;
        self.show_untracked = false;
        self.discovery_days_ago = 0;
        self.discovery_revision = self.discovery_revision.wrapping_add(1);
        self.refresh_queued = false;
        self.port_filters.clear();
        self.include_only_marked_ports = false;
        self.context_before = 0;
        self.context_after = 0;
        self.horizontal = 0;
        self.host_detail_offset = 0;
        self.host_pane_maximized = false;
        self.show_line_numbers = false;
        self.files_focus = self.sidebar_visible;
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.scroll_to_bottom();
        self.message = "View reset.".into();
    }

    pub fn cycle_leading_host(&mut self, backwards: bool) {
        if self.host_views.len() < 2 {
            return;
        }
        let current = self.leading_host_index().unwrap_or(0);
        let next = if backwards {
            (current + self.host_views.len() - 1) % self.host_views.len()
        } else {
            (current + 1) % self.host_views.len()
        };
        self.select_leading_host(next);
    }

    pub fn cycle_focus(&mut self, backwards: bool) {
        if self.combined || self.all_cached || self.host_views.len() < 2 {
            if self.sidebar_visible {
                self.files_focus = !self.files_focus;
            }
            return;
        }
        let last = self.host_views.len() - 1;
        if self.files_focus {
            self.select_leading_host(if backwards { last } else { 0 });
            self.files_focus = false;
            return;
        }
        let current = self.leading_host_index().unwrap_or(0);
        if self.sidebar_visible && ((!backwards && current == last) || (backwards && current == 0))
        {
            self.files_focus = true;
        } else {
            self.cycle_leading_host(backwards);
        }
    }

    /// Move the current log cursor, keeping it visible. Split host panes
    /// continue to center the shared cursor and align the other hosts to it.
    pub fn move_host_anchor(&mut self, delta: isize) -> bool {
        let Some(current) = self.anchor_position() else {
            return true;
        };
        let position = current
            .saturating_add_signed(delta)
            .min(self.matches.len() - 1);
        self.host_anchor_position = Some(position);
        self.cursor_time = self
            .matches
            .get(position)
            .and_then(|&row| self.rows.get(row))
            .and_then(|row| row.timestamp)
            .or(self.cursor_time);
        let height = self.view_height.max(1);
        if !self.combined && !self.all_cached && self.host_views.len() > 1 {
            self.scroll = position.saturating_sub(height / 2);
        } else if delta.unsigned_abs() >= height {
            self.scroll = position;
        } else if position < self.scroll {
            self.scroll = position;
        } else if position >= self.scroll.saturating_add(height) {
            self.scroll = position.saturating_add(1).saturating_sub(height);
        }
        true
    }

    pub fn move_host_anchor_to_edge(&mut self, end: bool) -> bool {
        if !self.matches.is_empty() {
            let position = if end { self.matches.len() - 1 } else { 0 };
            self.host_anchor_position = Some(position);
            self.cursor_time = self
                .matches
                .get(position)
                .and_then(|&row| self.rows.get(row))
                .and_then(|row| row.timestamp)
                .or(self.cursor_time);
            if !self.combined && !self.all_cached && self.host_views.len() > 1 {
                self.scroll = position.saturating_sub(self.view_height.max(1) / 2);
            } else {
                self.scroll = if end {
                    self.matches.len().saturating_sub(self.view_height.max(1))
                } else {
                    0
                };
            }
        }
        true
    }

    /// Forget collected content while keeping stream selections. The next
    /// catch-up starts at the current history window (today by default).
    pub fn clear_cached_logs(&mut self, rt: &Runtime) {
        self.cancel();
        self.scope_days = 1;
        self.save_subscriptions();
        let mut failures = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.cache_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(SUBSCRIPTIONS_FILE | CATALOG_FILE)
                ) {
                    continue;
                }
                if let Err(error) = fs::remove_file(&path) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        failures.push(format!("{}: {error}", path.display()));
                    }
                }
            }
        }
        self.snapshots.clear();
        self.parsed_streams.clear();
        self.row_sources.clear();
        self.rendered_records.borrow_mut().clear();
        self.host_views.clear();
        self.rows.clear();
        self.lines.clear();
        self.matches.clear();
        self.all_cached = false;
        self.scroll = 0;
        self.message = if failures.is_empty() {
            "Cleared cached logs; fetching today from now.".into()
        } else {
            format!(
                "Some cache files could not be cleared: {}",
                failures.join(" | ")
            )
        };
        self.sync(rt, None);
    }

    pub fn toggle_combined(&mut self) {
        let cursor = self.ensure_cursor_time();
        self.combined = !self.combined;
        self.files_focus = false;
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = cursor {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_visible = !self.sidebar_visible;
        self.files_focus = self.sidebar_visible;
    }

    pub fn option_items(&self) -> Vec<String> {
        if self.options_ports {
            return self
                .known_ports()
                .into_iter()
                .map(|port| {
                    let mark = match self.port_filters.get(&port) {
                        Some(PortFilter::Include) => "+ include (only marked +)",
                        Some(PortFilter::Exclude) => "- exclude",
                        None => "  all (no filter)",
                    };
                    format!("{port:>5}  {mark}")
                })
                .collect();
        }
        vec![
            format!(
                "Display: {}",
                if self.options.raw {
                    "raw source"
                } else {
                    "formatted"
                }
            ),
            format!(
                "Time: {}",
                if self.options.relative {
                    "relative to first record"
                } else {
                    "server clock"
                }
            ),
            format!(
                "Errors: {}",
                if self.options.full_errors {
                    "full stack and variables"
                } else {
                    "compact summary and stack"
                }
            ),
            format!("Wrap: {}", if self.options.wrap { "on" } else { "off" }),
            format!("Search: {}", self.options.search_mode.label()),
            format!(
                "ERR: {}",
                if self.options.kinds[0] {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            format!(
                "REQ: {}",
                if self.options.kinds[1] {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            format!(
                "ACC: {}",
                if self.options.kinds[2] {
                    "shown"
                } else {
                    "hidden"
                }
            ),
            format!(
                "Other: {}",
                if self.options.kinds[3] {
                    "shown"
                } else {
                    "hidden"
                }
            ),
        ]
    }
    pub fn change_option(&mut self) {
        if self.options_ports {
            if let Some(port) = self.known_ports().get(self.option_selected).cloned() {
                self.cycle_port(&port);
            }
            return;
        }
        match self.option_selected {
            0 => self.options.raw = !self.options.raw,
            1 => self.options.relative = !self.options.relative,
            2 => self.options.full_errors = !self.options.full_errors,
            3 => self.options.wrap = !self.options.wrap,
            4 => {
                self.options.search_mode = match self.options.search_mode {
                    SearchMode::Off => SearchMode::Highlight,
                    SearchMode::Highlight => SearchMode::Filter,
                    SearchMode::Filter => SearchMode::Off,
                }
            }
            i @ 5..=8 => self.options.kinds[i - 5] = !self.options.kinds[i - 5],
            _ => {}
        }
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        self.rebuild_lines();
        if at_bottom {
            self.scroll_to_bottom();
        }
    }
    fn known_ports(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|file| self.is_file_available(file))
            .filter_map(Self::file_port)
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn resize_content(&mut self, width: usize) {
        if self.view_width == width {
            return;
        }
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        self.view_width = width;
        if self.combined && self.options.wrap {
            self.rebuild_combined();
        }
        if at_bottom {
            self.scroll_to_bottom();
        }
    }

    fn rebuild_combined(&mut self) {
        self.combined_dirty = false;
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|file| {
                self.is_file_visible(file) && (self.all_cached || self.is_subscribed(file))
            })
            .filter(|file| {
                !self.include_only_marked_ports
                    || Self::file_port(file).is_some_and(|port| {
                        self.port_filters.get(port) == Some(&PortFilter::Include)
                    })
            })
            .cloned()
            .collect();
        for file in &files {
            let key = file.key();
            if let Some(snapshot) = self.snapshots.get(&key) {
                self.parsed_streams.entry(key).or_default().update(
                    &snapshot.file,
                    &snapshot.text,
                    false,
                    self.server_offset,
                );
            }
        }
        let floor = presentation::parse_time(&self.scope_start(), self.server_offset);
        let mut records: Vec<_> = files
            .iter()
            .filter_map(|file| {
                self.parsed_streams
                    .get(&file.key())
                    .map(|stream| (Arc::<str>::from(file.key()), stream))
            })
            .flat_map(|(key, stream)| {
                let range = stream.window_range(self.window.start, self.window.end);
                stream
                    .records
                    .iter()
                    .enumerate()
                    .skip(range.start)
                    .take(range.len())
                    .map(move |(index, record)| (key.clone(), index, record))
            })
            .filter(|(_, _, record)| self.window.contains(record.timestamp))
            .filter(|(_, _, record)| {
                self.options.kinds[match record.kind {
                    Kind::Error => 0,
                    Kind::Request => 1,
                    Kind::Access => 2,
                    Kind::Other => 3,
                }]
            })
            .filter(|(_, _, record)| {
                record
                    .timestamp
                    .is_none_or(|t| floor.is_none_or(|floor| t >= floor))
            })
            .collect();
        // Preserve deterministic tie order and genuine duplicate records.
        records.sort_by_key(|(_, _, record)| record.timestamp);
        let first = records.iter().find_map(|(_, _, record)| record.timestamp);
        let hosts = records
            .iter()
            .map(|(_, _, record)| &record.host)
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        self.first_time = first;
        self.show_hosts = hosts;
        self.rendered_records.borrow_mut().clear();
        let patterns = Patterns::new(&self.search);
        self.rows.clear();
        self.row_sources.clear();
        self.lines.clear();
        self.matches.clear();
        let mut previous_day = None;
        for (key, record_index, record) in records {
            // Search needs the formatted text; ordinary browsing does not.
            // Wrapping needs actual display widths to determine row offsets.
            let rendered = if !self.search.is_empty() || self.options.wrap {
                Some(record.render(&self.options, first, hosts))
            } else {
                None
            };
            let search_match = if self.search.is_empty() {
                true
            } else {
                let search_text = format!(
                    "{} {} {} {}",
                    record.host,
                    record.file,
                    record.raw,
                    rendered
                        .as_ref()
                        .expect("search requires rendering")
                        .iter()
                        .map(Line::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                patterns.matches(&search_text)
            };
            let matched = self.options.search_mode != SearchMode::Filter || search_match;
            if matched
                && let Some(day) = record.timestamp.map(|t| t.date())
                && previous_day != Some(day)
                && !self.options.relative
            {
                previous_day = Some(day);
                let line = Line::from(ratatui::text::Span::styled(
                    format!("── {day} (server time) ──"),
                    ratatui::style::Style::default().fg(ratatui::style::Color::White),
                ));
                self.matches.push(self.lines.len());
                self.lines.push(line.to_string());
                self.rows.push(ViewRow {
                    line,
                    timestamp: record.timestamp,
                    error: false,
                    start: false,
                    search_match,
                });
                self.row_sources.push(None);
            }
            if self.options.wrap {
                let mut line_index = 0;
                for line in rendered.expect("wrap requires rendering") {
                    let line = if self.options.search_mode == SearchMode::Off {
                        line
                    } else {
                        patterns.highlight(line)
                    };
                    let count = presentation::wrap_line(line, self.view_width.max(1)).len();
                    for _ in 0..count {
                        if matched {
                            self.matches.push(self.lines.len());
                        }
                        self.lines.push(String::new());
                        self.rows.push(ViewRow {
                            line: Line::default(),
                            timestamp: record.timestamp,
                            error: record.is_error(),
                            start: line_index == 0,
                            search_match,
                        });
                        self.row_sources.push(Some(RowSource {
                            key: key.clone(),
                            record: record_index,
                            line: line_index,
                            first,
                            raw_range: None,
                            wrapped: true,
                        }));
                        line_index += 1;
                    }
                }
            } else {
                let count = rendered
                    .as_ref()
                    .map_or_else(|| record.display_line_count(&self.options), Vec::len);
                for line in 0..count {
                    if matched {
                        self.matches.push(self.lines.len());
                    }
                    self.lines.push(String::new());
                    self.rows.push(ViewRow {
                        line: Line::default(),
                        timestamp: record.timestamp,
                        error: record.is_error(),
                        start: line == 0,
                        search_match,
                    });
                    self.row_sources.push(Some(RowSource {
                        key: key.clone(),
                        record: record_index,
                        line,
                        first,
                        raw_range: None,
                        wrapped: false,
                    }));
                }
            }
        }
        if !self.search.is_empty() && self.options.search_mode == SearchMode::Filter {
            self.expand_context();
        }
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
    }

    /// Timestamp at the visible cursor: the highlighted host cursor in split
    /// view, otherwise the first visible line (also highlighted by the UI).
    pub fn current_line_time(&self) -> Option<NaiveDateTime> {
        if !self.combined && !self.all_cached && self.host_views.len() > 1 {
            self.anchor_timestamp()
        } else {
            let index = *self.matches.get(self.anchor_position()?)?;
            self.rows
                .get(index)
                .and_then(|row| row.timestamp)
                .or_else(|| {
                    let source = self.row_sources.get(index)?.as_ref()?;
                    let (start, end) = source.raw_range?;
                    let snapshot = self.snapshots.get(source.key.as_ref())?;
                    let current = snapshot.text.get(start..end)?;
                    presentation::line_time(&snapshot.file, current, self.server_offset).or_else(
                        || {
                            snapshot.text.get(..start)?.lines().rev().find_map(|line| {
                                presentation::line_time(&snapshot.file, line, self.server_offset)
                            })
                        },
                    )
                })
        }
    }

    pub fn set_truncation_at_cursor(&mut self, start: bool) {
        if self.files_focus {
            self.message = "Focus a log line with Enter or Tab first.".into();
            return;
        }
        let Some(time) = self.current_line_time() else {
            self.message =
                "The current line has no timestamp; select a timestamped log line.".into();
            return;
        };
        let mut window = self.window;
        if start {
            window.start = Some(time);
        } else {
            window.end = Some(time);
        }
        if window.start.zip(window.end).is_some_and(|(a, b)| a > b) {
            self.message =
                "Start must not be after end. Adjust the other bound with Ctrl+T.".into();
            return;
        }
        self.cursor_time = Some(time);
        self.apply_window(window, Some(time));
    }

    fn apply_window(&mut self, window: TimeWindow, anchor: Option<NaiveDateTime>) {
        self.window = window;
        self.default_truncation = false;
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = anchor {
            self.seek_nearest(time);
        } else {
            self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
        }
        self.message = if window.active() {
            format!("Truncation active: {}", self.window_label())
        } else {
            "Truncation cleared.".into()
        };
    }

    pub fn window_label(&self) -> String {
        let format_time = |time: Option<NaiveDateTime>| {
            time.map(|t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_else(|| "open".into())
        };
        format!(
            "{} to {} (server time)",
            format_time(self.window.start),
            format_time(self.window.end)
        )
    }

    pub fn open_truncation_dialog(&mut self) {
        let format_time = |time: Option<NaiveDateTime>| {
            time.map(|t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_default()
        };
        self.truncation_dialog = Some(TruncationDialog {
            fields: [
                format_time(self.window.start),
                format_time(self.window.end),
                "5".into(),
            ],
            selected: 0,
            error: String::new(),
        });
    }

    pub fn adjust_truncation_dialog(&mut self, forward: bool) {
        let Some(dialog) = self.truncation_dialog.as_mut() else {
            return;
        };
        if dialog.selected > 1 {
            dialog.error = "Select start or end to adjust it.".into();
            return;
        }
        let minutes: i64 = match dialog.fields[2].trim().parse() {
            Ok(n) if (1..=5256000).contains(&n) => n,
            _ => {
                dialog.error = "Minutes must be a positive number (up to 5256000).".into();
                return;
            }
        };
        let selected = dialog.selected;
        let Some(time) =
            presentation::parse_time(dialog.fields[selected].trim(), self.server_offset)
        else {
            dialog.error = "Enter a start or end timestamp before adjusting it.".into();
            return;
        };
        let delta = Days::minutes(if forward { minutes } else { -minutes });
        let Some(time) = time.checked_add_signed(delta) else {
            dialog.error = "Adjusted timestamp is out of range.".into();
            return;
        };
        dialog.fields[selected] = time.format("%Y-%m-%d %H:%M:%S%.3f").to_string();
        dialog.error.clear();
    }

    pub fn apply_truncation_dialog(&mut self) {
        let Some(dialog) = self.truncation_dialog.as_ref() else {
            return;
        };
        let parse = |value: &str| -> Result<Option<NaiveDateTime>, String> {
            if value.trim().is_empty() {
                Ok(None)
            } else {
                presentation::parse_time(value.trim(), self.server_offset)
                    .map(Some)
                    .ok_or_else(|| "Use YYYY-MM-DD HH:MM:SS[.mmm], or leave a bound empty.".into())
            }
        };
        let window = match (parse(&dialog.fields[0]), parse(&dialog.fields[1])) {
            (Ok(start), Ok(end)) if !start.zip(end).is_some_and(|(a, b)| a > b) => {
                TimeWindow { start, end }
            }
            (Ok(_), Ok(_)) => {
                self.truncation_dialog.as_mut().unwrap().error =
                    "Start must not be after end.".into();
                return;
            }
            (Err(error), _) | (_, Err(error)) => {
                self.truncation_dialog.as_mut().unwrap().error = error;
                return;
            }
        };
        let anchor = self.current_line_time();
        if let Some(time) = anchor {
            self.cursor_time = Some(time);
        }
        self.truncation_dialog = None;
        self.apply_window(window, anchor);
    }

    pub fn seek_time(&mut self) {
        let input = self.time_input.trim();
        let input = if input.len() <= 12 {
            format!("{} {input}", self.now().date())
        } else {
            input.to_string()
        };
        match presentation::parse_time(&input, self.server_offset) {
            Some(target) => {
                self.cursor_time = Some(target);
                self.host_anchor_position = None;
                self.seek_nearest(target);
                self.cursor_time = self.current_line_time().or(self.cursor_time);
                self.time_editing = false;
            }
            None => self.message = "Use HH:MM:SS or YYYY-MM-DD HH:MM:SS (server time).".into(),
        }
    }
    fn seek_nearest(&mut self, target: NaiveDateTime) {
        if let Some((position, _)) = self
            .matches
            .iter()
            .enumerate()
            .filter_map(|(i, &row)| {
                self.rows
                    .get(row)
                    .and_then(|r| r.timestamp)
                    .map(|time| (i, (time - target).num_milliseconds().unsigned_abs()))
            })
            .min_by_key(|(_, diff)| *diff)
        {
            self.host_anchor_position = Some(position);
            self.scroll = if !self.combined && !self.all_cached && self.host_views.len() > 1 {
                position.saturating_sub(self.view_height.max(1) / 2)
            } else {
                position.min(self.matches.len().saturating_sub(self.view_height.max(1)))
            };
        }
    }
    pub fn jump_time(&mut self, seconds: i64) {
        if let Some(time) = self.current_line_time() {
            let target = time + Days::seconds(seconds);
            self.cursor_time = Some(target);
            self.host_anchor_position = None;
            self.seek_nearest(target);
            self.cursor_time = self.current_line_time().or(self.cursor_time);
        }
    }
    pub fn jump_record(&mut self, backwards: bool, errors: bool) {
        let eligible: Vec<_> = self
            .matches
            .iter()
            .enumerate()
            .filter_map(|(i, &row)| {
                let r = self.rows.get(row)?;
                (r.start && if errors { r.error } else { r.search_match }).then_some(i)
            })
            .collect();
        let current = self.anchor_position().unwrap_or(self.scroll);
        let next = if backwards {
            eligible
                .iter()
                .rev()
                .copied()
                .find(|&i| i < current)
                .or_else(|| eligible.last().copied())
        } else {
            eligible
                .iter()
                .copied()
                .find(|&i| i > current)
                .or_else(|| eligible.first().copied())
        };
        if let Some(position) = next {
            self.scroll = position;
            self.host_anchor_position = Some(position);
            self.cursor_time = self
                .matches
                .get(position)
                .and_then(|&row| self.rows.get(row))
                .and_then(|row| row.timestamp)
                .or(self.cursor_time);
        }
    }
    pub fn visible_text(&self) -> String {
        self.matches
            .iter()
            .map(|&i| self.display_row(i).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
    pub fn save_view(&mut self) {
        let path = self.cache_root.join(format!(
            "view-{}.txt",
            chrono::Local::now().format("%Y%m%d-%H%M%S-%f")
        ));
        self.message = match fs::write(&path, self.visible_text()) {
            Ok(()) => format!("Saved {}", path.display()),
            Err(e) => format!("Saving view: {e}"),
        };
    }

    /// Export a stable copy for an editor without reading the editor's result
    /// back into the cache. A raw single-file view can be copied directly.
    pub fn editor_export(&self) -> Result<PathBuf> {
        let path = crate::external_editor::temp_editor_path(Some(Path::new("logs.txt")));
        if !self.combined
            && !self.all_cached
            && self.search.is_empty()
            && !self.window.active()
            && let Some(source) = self.local_path()
            && let Some(snapshot) = self
                .selected_file()
                .and_then(|file| self.snapshots.get(&file.key()))
        {
            let mut input = fs::File::open(source)?.take(snapshot.text_bytes);
            let mut output = fs::File::create(&path)?;
            std::io::copy(&mut input, &mut output)?;
        } else {
            let mut output = std::io::BufWriter::new(fs::File::create(&path)?);
            for (position, index) in self.matches.iter().enumerate() {
                if position > 0 {
                    output.write_all(b"\n")?;
                }
                output.write_all(self.display_row(*index).to_string().as_bytes())?;
            }
            output.flush()?;
        }
        Ok(path)
    }
    #[cfg(test)]
    pub fn cached_total(&self) -> u64 {
        self.snapshots.values().map(|s| s.text.len() as u64).sum()
    }

    pub fn refresh(&mut self, rt: &Runtime) {
        if self.busy {
            self.refresh_queued = true;
            self.message = "Discovery queued until the current log sync finishes…".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        let progress = self.begin_activity("Discovering logs", 0);
        self.message = "Discovering hosts and log sizes…".into();
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let tracked_names = self.tracked_names();
        let days_ago = self.discovery_days_ago;
        // The compact port/type grid is small enough to show the complete
        // discovered inventory, including logs that are not being collected.
        let show_untracked = true;
        let revision = self.discovery_revision;
        let handle = rt.spawn(async move {
            let mut accounted = counter.load(Ordering::Relaxed);
            let client = client
                .with_download_counter(counter.clone())
                .with_download_progress(progress);
            let result = client.logs_eval(DISCOVER, &json!({})).await.and_then(|v| {
                serde_json::from_value::<Discovery>(v).context("Invalid cluster discovery")
            });
            let discovery = match result {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(CollectorEvent::Report {
                        host: client.server.host.clone(),
                        result: Err(format!("{e:#}")),
                        bytes: take_delta(&counter, &mut accounted),
                    });
                    let _ = tx.send(CollectorEvent::Done { auto_tail: false });
                    return;
                }
            };
            let _ = tx.send(CollectorEvent::Clock(discovery.now));
            let discovery_bytes = take_delta(&counter, &mut accounted);
            let mut endpoints = Vec::new();
            let mut hosts = discovery.hosts;
            hosts.sort();
            hosts.dedup();
            for host in hosts {
                // Cluster file URLs route directory reads and file reads to
                // this host, including nodes unreachable from the workstation.
                endpoints.push(Endpoint {
                    host,
                    client: client.clone(),
                });
            }
            let _ = tx.send(CollectorEvent::Endpoints(endpoints.clone()));
            for (index, endpoint) in endpoints.iter().enumerate() {
                let result = inventory(
                    endpoint,
                    &directory,
                    days_ago,
                    show_untracked,
                    &tracked_names,
                )
                .await
                .map_err(|e| format!("{e:#}"));
                let bytes = take_delta(&counter, &mut accounted)
                    + if index == 0 { discovery_bytes } else { 0 };
                let _ = tx.send(CollectorEvent::Inventory {
                    host: endpoint.host.clone(),
                    result,
                    bytes,
                    revision,
                    days_ago,
                    show_untracked,
                });
            }
            let _ = tx.send(CollectorEvent::Done { auto_tail: true });
        });
        self.abort = Some(handle.abort_handle());
    }

    pub fn toggle_discovery(&mut self, rt: &Runtime) {
        self.show_untracked = !self.show_untracked;
        if self.show_untracked {
            self.discovery_days_ago = 0;
        }
        self.discovery_revision = self.discovery_revision.wrapping_add(1);
        self.ensure_visible_selection();
        self.refresh(rt);
    }

    /// Download every available host's copy of the selected file in full.
    pub fn download_full(&mut self, rt: &Runtime) {
        if self.busy {
            self.message =
                "A log operation is already running; retry the full download when it finishes."
                    .into();
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for source in self.files.iter().filter(|source| source.name == file.name) {
            let request = self.snapshots.get(&source.key()).map(|snapshot| snapshot.request(true))
                .unwrap_or_else(|| json!({"path":source.path, "offset":-1, "anchor_start":0, "anchor":"", "full":true}));
            requests
                .entry(source.host.clone())
                .or_default()
                .push(request);
        }
        let plans: Vec<_> = self
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                requests
                    .remove(&endpoint.host)
                    .map(|requests| (endpoint.clone(), requests))
            })
            .collect();
        if plans.is_empty() {
            self.message =
                "Hosts unavailable; r retries discovery. Cached logs remain searchable.".into();
            return;
        }
        if matches!(
            self.tracking_mode(&file),
            TrackingMode::Untracked | TrackingMode::Partial
        ) {
            self.modes.insert(file.name.clone(), TrackingMode::OnDemand);
            self.save_subscriptions();
        }
        self.start_downloads(rt, plans, "Downloading logs");
    }

    /// Refresh the selected filename across hosts according to its mode.
    pub fn download(&mut self, rt: &Runtime) {
        if let Some(file) = self.selected_file().cloned() {
            match self.tracking_mode(&file) {
                TrackingMode::OnDemand => self.sync_on_demand(rt, Some(file.name.clone())),
                TrackingMode::Live | TrackingMode::Partial => {
                    self.sync(rt, Some(file.name.clone()))
                }
                TrackingMode::Untracked => {
                    self.message = "Choose a tracking mode with o first.".into()
                }
            }
        }
    }

    pub fn sync_on_demand(&mut self, rt: &Runtime, only: Option<String>) {
        if self.busy {
            if let Some(key) = only {
                self.demand_files_queued.insert(key);
            } else {
                self.demand_queued = true;
                self.demand_files_queued.clear();
            }
            self.message = "On-demand refresh queued.".into();
            return;
        }
        let mut requests = self.on_demand_requests(only.as_deref());
        let plans: Vec<_> = self
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                requests
                    .remove(&endpoint.host)
                    .map(|requests| (endpoint.clone(), requests))
            })
            .collect();
        if plans.is_empty() {
            self.message = "No on-demand logs available to refresh.".into();
            return;
        }
        self.start_downloads(rt, plans, "Refreshing logs");
    }

    fn start_downloads(
        &mut self,
        rt: &Runtime,
        plans: Vec<(Endpoint, Vec<Value>)>,
        label: &'static str,
    ) {
        self.download_results.clear();
        let mut jobs = Vec::new();
        for (endpoint, requests) in plans {
            self.errors.remove(&endpoint.host);
            for request in requests {
                let path = request["path"].as_str().unwrap_or_default();
                let name = path.rsplit('/').next().unwrap_or(path).to_owned();
                let size = self
                    .files
                    .iter()
                    .find(|file| file.host == endpoint.host && file.path == path)
                    .map_or(u64::MAX, |file| file.size);
                jobs.push((size, endpoint.clone(), name, request));
            }
        }
        // Finish small files promptly rather than making them wait for a large
        // file included in the same host response.
        jobs.sort_by_key(|job| job.0);
        let progress = self.begin_activity(label, jobs.len());
        self.activity.as_mut().unwrap().download = true;
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let handle = rt.spawn(async move {
            let mut tasks = JoinSet::new();
            let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
            for (_, endpoint, file, request) in jobs {
                let (tx, directory, counter, progress, permits) = (
                    tx.clone(),
                    directory.clone(),
                    counter.clone(),
                    progress.clone(),
                    permits.clone(),
                );
                tasks.spawn(async move {
                    let _permit = permits
                        .acquire_owned()
                        .await
                        .expect("download semaphore remains open");
                    let _ = tx.send(CollectorEvent::DownloadStarted {
                        host: endpoint.host.clone(),
                        file: file.clone(),
                    });
                    let local = Arc::new(AtomicU64::new(0));
                    let endpoint = Endpoint {
                        host: endpoint.host,
                        client: endpoint
                            .client
                            .with_download_counter(local.clone())
                            .with_download_progress(progress),
                    };
                    let result = collect_since(&endpoint, &directory, &[request], "")
                        .await
                        .map_err(|error| format!("{error:#}"));
                    let bytes = local.load(Ordering::Relaxed);
                    counter.fetch_add(bytes, Ordering::Relaxed);
                    let _ = tx.send(CollectorEvent::DownloadReport {
                        host: endpoint.host,
                        file,
                        result,
                        bytes,
                    });
                });
            }
            while tasks.join_next().await.is_some() {}
            let _ = tx.send(CollectorEvent::Done { auto_tail: false });
        });
        self.abort = Some(handle.abort_handle());
    }

    fn on_demand_requests(&self, only: Option<&str>) -> BTreeMap<String, Vec<Value>> {
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for file in &self.files {
            if self.tracking_mode(file) != TrackingMode::OnDemand
                || only.is_some_and(|key| key != file.key() && key != file.name)
            {
                continue;
            }
            let full_size = self
                .snapshots
                .get(&file.key())
                .and_then(|snapshot| snapshot.full_size);
            requests.entry(file.host.clone()).or_default().push(json!({
                "path": file.path, "offset": -1, "anchor_start": 0, "anchor": "",
                "full": true, "conditional": full_size.is_some(), "known_size": full_size
            }));
        }
        requests
    }

    /// Per-host work: every tracked, present file (or just `only`). Error logs
    /// are fetched by time window; others fall back to the eval collector.
    fn plan(&self, only: Option<&str>) -> Vec<HostPlan> {
        let scope = self.scope_start();
        let mut plans = Vec::new();
        for endpoint in &self.endpoints {
            let mut plan = HostPlan {
                host: endpoint.host.clone(),
                client: Some(endpoint.client.clone()),
                ..HostPlan::default()
            };
            for file in self.files.iter().filter(|f| f.host == endpoint.host) {
                let key = file.key();
                let snapshot = self.snapshots.get(&key);
                let wanted = match only {
                    Some(only) => only == key || only == file.name,
                    None => self.tracking_mode(file) == TrackingMode::Live,
                };
                if !wanted {
                    continue;
                }
                if is_error_log(&file.name) {
                    plan.timed.push(timed_request(file, snapshot, &scope));
                } else {
                    plan.legacy.push(snapshot.map(|s| s.request(false)).unwrap_or_else(|| {
                        json!({"path":file.path, "offset":-1, "anchor_start":0, "anchor":"", "full":false})
                    }));
                }
            }
            if !plan.timed.is_empty() || !plan.legacy.is_empty() {
                plans.push(plan);
            }
        }
        plans
    }

    /// Catch every tracked stream up to now, concurrently. Streams resume from
    /// their saved cursor, so a long absence costs one request per stream.
    pub fn sync(&mut self, rt: &Runtime, only: Option<String>) {
        if self.busy {
            return;
        }
        let plans = self.plan(only.as_deref());
        let scope = self.scope_start();
        if plans.is_empty() {
            self.message =
                "Nothing to fetch yet. r refreshes the inventory; o tracks a file.".into();
            return;
        }
        let total = plans
            .iter()
            .map(|plan| plan.timed.len() + usize::from(!plan.legacy.is_empty()))
            .sum();
        let progress = self.begin_activity("Updating logs", total);
        self.message = "Catching up tracked logs…".into();
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let handle = rt.spawn(async move {
            // Dropping the set (cancel) aborts every in-flight request.
            let mut tasks = JoinSet::new();
            for plan in plans {
                let Some(client) = plan.client else { continue };
                for request in plan.timed {
                    let (tx, counter, client) = (tx.clone(), counter.clone(), client.clone());
                    let directory = directory.clone();
                    let progress = progress.clone();
                    tasks.spawn(async move {
                        let local = Arc::new(AtomicU64::new(0));
                        let client = client
                            .with_download_counter(local.clone())
                            .with_download_progress(progress);
                        let chunk = fetch_timed(&client, &directory, request).await;
                        let bytes = local.load(Ordering::Relaxed);
                        counter.fetch_add(bytes, Ordering::Relaxed);
                        let _ = tx.send(CollectorEvent::Timed {
                            host: chunk.request.host.clone(),
                            chunk: Box::new(chunk),
                            bytes,
                        });
                    });
                }
                if !plan.legacy.is_empty() {
                    let progress = progress.clone();
                    let (tx, counter, directory, scope) = (
                        tx.clone(),
                        counter.clone(),
                        directory.clone(),
                        scope.clone(),
                    );
                    tasks.spawn(async move {
                        let local = Arc::new(AtomicU64::new(0));
                        let endpoint = Endpoint {
                            host: plan.host.clone(),
                            client: client
                                .with_download_counter(local.clone())
                                .with_download_progress(progress),
                        };
                        let result = collect_since(&endpoint, &directory, &plan.legacy, &scope)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let bytes = local.load(Ordering::Relaxed);
                        counter.fetch_add(bytes, Ordering::Relaxed);
                        let _ = tx.send(CollectorEvent::Report {
                            host: plan.host,
                            result,
                            bytes,
                        });
                    });
                }
            }
            while tasks.join_next().await.is_some() {}
            let _ = tx.send(CollectorEvent::Done { auto_tail: false });
        });
        self.abort = Some(handle.abort_handle());
    }

    pub fn tick(&mut self, rt: &Runtime, visible: bool) {
        // Content pinned to the bottom stays pinned; a user who scrolled up
        // is not yanked back by background updates.
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        while let Ok(event) = self.rx.try_recv() {
            match event {
                CollectorEvent::DownloadStarted { host, file } => {
                    if let Some(activity) = &mut self.activity {
                        activity.active.insert((host, file));
                    }
                }
                CollectorEvent::DownloadReport {
                    host,
                    file,
                    result,
                    bytes,
                } => {
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host.clone()).or_default() += bytes;
                    let previous = self.errors.remove(&host);
                    let failure = match result {
                        Ok(response) => match self.apply(response) {
                            Ok(()) => self.errors.remove(&host),
                            Err(error) => Some(format!("Cache error: {error:#}")),
                        },
                        Err(error) => Some(error),
                    };
                    if let Some(activity) = &mut self.activity {
                        activity.active.remove(&(host.clone(), file.clone()));
                        activity.completed += 1;
                        activity.failed += usize::from(failure.is_some());
                        activity.last = Some((host.clone(), file.clone(), failure.is_some()));
                    }
                    let current = failure.map(|error| format!("{file}: {error}"));
                    self.download_results
                        .insert((host.clone(), file), current.is_none());
                    if let Some(error) = match (previous, current) {
                        (Some(previous), Some(current)) => Some(format!("{previous}; {current}")),
                        (Some(error), None) | (None, Some(error)) => Some(error),
                        (None, None) => None,
                    } {
                        self.errors.insert(host, error);
                    }
                }
                CollectorEvent::Endpoints(endpoints) => {
                    if let Some(activity) = &mut self.activity {
                        activity.total = endpoints.len();
                    }
                    self.endpoints = endpoints;
                }
                CollectorEvent::Clock(now) => {
                    if let Ok(now) = DateTime::parse_from_rfc3339(&now) {
                        let offset_changed = self.server_offset != *now.offset();
                        if offset_changed {
                            self.parsed_streams.clear();
                        }
                        self.server_offset = *now.offset();
                        self.clock = Some((now.naive_local(), Instant::now()));
                        let corrected_default = self.default_truncation;
                        if corrected_default {
                            self.window.start = Some(now.naive_local());
                            self.default_truncation = false;
                        }
                        if (offset_changed || corrected_default) && !self.combined {
                            self.rebuild_lines();
                        }
                        self.combined_dirty = self.combined;
                    }
                }
                CollectorEvent::Timed { host, chunk, bytes } => {
                    self.complete_activity_step();
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host.clone()).or_default() += bytes;
                    if let Err(e) = self.apply_timed(*chunk, at_bottom) {
                        self.errors.insert(host, format!("Cache error: {e:#}"));
                    }
                }
                CollectorEvent::Report {
                    host,
                    result,
                    bytes,
                } => {
                    self.complete_activity_step();
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host.clone()).or_default() += bytes;
                    match result {
                        Ok(response) => {
                            if let Err(e) = self.apply(response) {
                                self.errors.insert(host, format!("Cache error: {e:#}"));
                            }
                        }
                        Err(error) => {
                            self.errors.insert(host.clone(), error.clone());
                            self.message = format!("{host}: {error}");
                        }
                    }
                }
                CollectorEvent::Inventory {
                    host,
                    result,
                    bytes,
                    revision,
                    days_ago,
                    show_untracked,
                } => {
                    self.complete_activity_step();
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host.clone()).or_default() += bytes;
                    if revision != self.discovery_revision {
                        continue;
                    }
                    match result {
                        Ok(response) => {
                            if show_untracked {
                                let stale: BTreeSet<_> = self
                                    .files
                                    .iter()
                                    .filter(|file| {
                                        file.host == host
                                            && self.tracking_mode(file) == TrackingMode::Untracked
                                            && Self::file_days_ago(file) == Some(days_ago)
                                    })
                                    .map(LogFile::key)
                                    .collect();
                                for file in &mut self.files {
                                    if stale.contains(&file.key()) {
                                        file.present = false;
                                    }
                                }
                            }
                            if let Err(error) = self.apply(response) {
                                self.errors.insert(host, format!("Cache error: {error:#}"));
                            }
                        }
                        Err(error) => {
                            self.errors.insert(host.clone(), error.clone());
                            self.message = format!("{host}: {error}");
                        }
                    }
                }
                CollectorEvent::Done { auto_tail } => {
                    self.busy = false;
                    let finished = self.activity.take();
                    self.abort = None;
                    self.last_poll = Instant::now();
                    let refresh_queued = self.refresh_queued;
                    self.refresh_queued = false;
                    if refresh_queued {
                        self.refresh(rt);
                    } else if self.demand_queued {
                        self.demand_queued = false;
                        self.demand_files_queued.clear();
                        self.sync_on_demand(rt, None);
                    } else if let Some(key) = self.demand_files_queued.pop_first() {
                        self.sync_on_demand(rt, Some(key));
                    } else if auto_tail
                        && visible
                        && self
                            .files
                            .iter()
                            .any(|file| self.tracking_mode(file) == TrackingMode::Live)
                    {
                        self.sync(rt, None);
                    } else if let Some(activity) = finished.filter(|activity| activity.download) {
                        self.message = format!(
                            "{} finished: {} successful, {} failed",
                            activity.label,
                            activity.completed.saturating_sub(activity.failed),
                            activity.failed
                        );
                    } else if self.errors.is_empty() {
                        self.message = format!(
                            "Up to date ({}) — {} tracked online",
                            self.scope_label(),
                            self.files
                                .iter()
                                .filter(|f| f.present && self.is_subscribed(f))
                                .count()
                        );
                    }
                }
            }
        }
        self.body_bytes = self.counter.load(Ordering::Relaxed);
        // A scan can deliver many chunks from many streams in one event-loop
        // pass. Render that batch once, retaining an operator's review time
        // when older records arrive out of order.
        if self.combined && self.combined_dirty {
            let reviewing = (!at_bottom)
                .then(|| {
                    self.matches
                        .get(self.scroll)
                        .and_then(|&row| self.rows.get(row))
                        .and_then(|row| row.timestamp)
                })
                .flatten();
            self.rebuild_combined();
            if self.following && at_bottom {
                self.scroll_to_bottom();
            } else if let Some(time) = reviewing {
                self.seek_nearest(time);
            }
        }
        if !self.busy && self.last_poll.elapsed() >= INTERVAL {
            self.last_poll = Instant::now();
            if self
                .files
                .iter()
                .any(|f| self.tracking_mode(f) == TrackingMode::Live)
            {
                self.sync(rt, None);
            }
        }
    }

    /// Append `text` to a file's snapshot (or start a new generation) so that
    /// local work is proportional to the new data: the content is appended to
    /// the `.log` file and only small cursor metadata is rewritten. A crash
    /// between the two leaves the cursor at the previous committed length.
    /// Returns the prior text length for incremental view updates.
    fn commit(
        &mut self,
        file: LogFile,
        fresh: bool,
        text: &str,
        update: impl FnOnce(&mut Snapshot),
    ) -> Result<usize> {
        let key = file.key();
        let stem = hash_key(&key);
        let prior = self.snapshots.get(&key);
        let prior_generation = prior.map(|s| s.generation);
        let (mut meta, base) = match prior {
            Some(prior) if !fresh => (prior.meta(), prior.text.len()),
            _ => (
                Snapshot {
                    file: file.clone(),
                    start: 0,
                    end: 0,
                    length: 0,
                    anchor_start: 0,
                    anchor: String::new(),
                    text: String::new(),
                    text_bytes: 0,
                    generation: prior_generation.map(|g| g + 1).unwrap_or(0),
                    time: None,
                    full_size: None,
                    updated_at: None,
                },
                0,
            ),
        };
        meta.file = file;
        update(&mut meta);
        meta.text_bytes = (base + text.len()) as u64;
        fs::create_dir_all(&self.cache_root)?;
        let log = self.cache_root.join(format!("{stem}.log"));
        if fresh && let Some(generation) = prior_generation {
            // Preserve the prior generation (rotation, truncation, explicit
            // redownload) by renaming: no rewrite of the old content.
            let previous = self
                .cache_root
                .join(format!("{stem}.{generation}.previous.log"));
            match fs::rename(&log, &previous) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).context("Preserving previous log generation"),
            }
        }
        let mut out = fs::OpenOptions::new();
        out.create(true);
        if fresh {
            out.write(true).truncate(true);
        } else {
            out.append(true);
        }
        let mut out = out
            .open(&log)
            .with_context(|| format!("Opening {}", log.display()))?;
        out.write_all(text.as_bytes())?;
        drop(out);
        if fresh || !text.is_empty() {
            meta.updated_at = Some(Utc::now().to_rfc3339());
        }
        atomic_write(
            &self.cache_root.join(format!("{stem}.json")),
            &serde_json::to_vec(&meta)?,
        )?;
        if fresh {
            self.parsed_streams.remove(&key);
        }
        match self.snapshots.get_mut(&key) {
            Some(existing) if !fresh => {
                meta.text = std::mem::take(&mut existing.text);
                meta.text.push_str(text);
                *existing = meta;
            }
            _ => {
                meta.text = text.to_owned();
                self.snapshots.insert(key, meta);
            }
        }
        Ok(base)
    }

    /// Keep the selected view in step with an append while reusing the
    /// incremental parser state for the stream.
    fn extend_view(&mut self, key: &str, fresh: bool, _old_len: usize) {
        if fresh {
            self.parsed_streams.remove(key);
        }
        if self.combined {
            self.combined_dirty = true;
            return;
        }
        let selected_name = self.selected_file().map(|file| file.name.as_str());
        let changed = self
            .files
            .iter()
            .find(|file| file.key() == key)
            .map(|file| file.name.as_str());
        if self.all_cached || selected_name == changed {
            self.rebuild_lines();
            return;
        }
    }

    fn apply_timed(&mut self, chunk: TimedChunk, at_bottom: bool) -> Result<()> {
        let request = chunk.request;
        if let Some(error) = chunk.error {
            self.errors
                .insert(request.host.clone(), format!("{}: {error}", request.name));
            return Ok(());
        }
        self.update_timed_file_metadata(
            &request.host,
            &request.path,
            chunk.size,
            chunk.modified.as_deref(),
        )?;
        self.errors.remove(&request.host);
        let (Some(last_ts), false) = (chunk.last_ts, chunk.text.is_empty()) else {
            return Ok(());
        };
        let file = self
            .files
            .iter()
            .find(|f| f.host == request.host && f.path == request.path)
            .context("Chunk has no matching file metadata")?
            .clone();
        let key = file.key();
        let held = self.snapshots.get(&key);
        let fresh = held.is_none() || request.reset;
        let text = chunk.text;
        let since = match held.and_then(|s| s.time.as_ref()) {
            Some(time) if !fresh => time.since.clone(),
            _ => request.start.clone(),
        };
        let appended_chars = text.chars().count() as u64;
        let old_len = self.commit(file, fresh, &text, |s| {
            if fresh {
                s.end = 0;
            }
            s.end += appended_chars;
            s.length = s.end;
            s.time = Some(TimeCursor {
                ts: last_ts,
                count: chunk.last_count,
                since,
            });
        })?;
        self.extend_view(&key, fresh, old_len);
        self.sort_files();
        self.finish_update(at_bottom);
        Ok(())
    }

    fn update_timed_file_metadata(
        &mut self,
        host: &str,
        path: &str,
        size: Option<u64>,
        modified: Option<&str>,
    ) -> Result<()> {
        if size.is_none() && modified.is_none() {
            return Ok(());
        }
        let Some(file) = self
            .files
            .iter_mut()
            .find(|file| file.host == host && file.path == path)
        else {
            return Ok(());
        };
        if let Some(size) = size {
            file.size = size;
        }
        if let Some(modified) = modified {
            file.modified = modified.to_owned();
        }
        let key = file.key();
        let file = file.clone();
        if let Some(snapshot) = self.snapshots.get_mut(&key) {
            snapshot.file = file;
            let meta = snapshot.meta();
            atomic_write(
                &self.cache_root.join(format!("{}.json", hash_key(&key))),
                &serde_json::to_vec(&meta)?,
            )?;
        }
        Ok(())
    }

    fn finish_update(&mut self, at_bottom: bool) {
        self.ensure_visible_selection();
        if self.following && at_bottom {
            self.scroll_to_bottom();
        }
    }

    fn apply(&mut self, response: HostResponse) -> Result<()> {
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        let old_selected = self.selected_file().map(LogFile::key);
        let returned_files = response.files.len();
        self.read_bytes += response.read_bytes;
        self.cluster_bytes += response.cluster_bytes;
        self.errors.remove(&response.host);
        for file in &mut self.files {
            if response.inventory && file.host == response.host {
                file.present = false;
            }
        }
        let mut indices: HashMap<String, usize> = self
            .files
            .iter()
            .enumerate()
            .map(|(index, file)| (file.key(), index))
            .collect();
        for mut file in response.files {
            file.present = true;
            let key = file.key();
            if let Some(&index) = indices.get(&key) {
                self.files[index] = file;
            } else {
                indices.insert(key, self.files.len());
                self.files.push(file);
            }
        }
        let mut errors = Vec::new();
        let mut cached_chunks = 0;
        let mut updated = Vec::new();
        for chunk in response.chunks {
            if let Some(error) = &chunk.error {
                errors.push(format!("{}: {}", chunk.path, error));
                continue;
            }
            let file = self
                .files
                .iter()
                .find(|f| f.host == chunk.host && f.path == chunk.path)
                .context("Chunk has no matching file metadata")?
                .clone();
            let key = file.key();
            validate_chunk(&file, &chunk)?;
            let previous = self.snapshots.get(&key);
            let fresh = chunk.reset || previous.is_none();
            if !fresh && previous.is_some_and(|p| p.end != chunk.start) {
                bail!("Non-contiguous log chunk; refresh before retrying");
            }
            let old_len = self.commit(file, fresh, &chunk.text, |s| {
                if fresh {
                    s.start = chunk.start;
                }
                s.end = chunk.end;
                s.length = chunk.length;
                s.anchor_start = chunk.anchor_start;
                s.anchor = chunk.anchor.clone();
                if chunk.full && chunk.text.len() as u64 == s.file.size {
                    s.full_size = Some(s.file.size);
                }
            })?;
            updated.push((key, fresh, old_len));
            cached_chunks += 1;
        }
        self.sort_files();
        if let Some(ref key) = old_selected {
            self.selected = self.files.iter().position(|f| f.key() == *key).unwrap_or(0);
        }
        if old_selected != self.selected_file().map(LogFile::key) {
            self.rebuild_lines();
            self.scroll_to_bottom();
        } else {
            for (key, fresh, old_len) in updated {
                self.extend_view(&key, fresh, old_len);
            }
        }
        self.finish_update(at_bottom);
        if !self.cache_root.as_os_str().is_empty() {
            fs::create_dir_all(&self.cache_root)?;
            atomic_write(
                &self.cache_root.join(CATALOG_FILE),
                &serde_json::to_vec(&self.files)?,
            )?;
        }
        if !errors.is_empty() {
            self.errors.insert(response.host.clone(), errors.join("; "));
        }
        self.message = if errors.is_empty() {
            format!(
                "{}: {} files returned from {}{}",
                response.host,
                returned_files,
                response.directory,
                if cached_chunks > 0 { " — cached" } else { "" }
            )
        } else {
            errors.join("; ")
        };
        Ok(())
    }
}

async fn inventory(
    endpoint: &Endpoint,
    directory: &str,
    days_ago: u32,
    show_untracked: bool,
    tracked_names: &BTreeSet<String>,
) -> Result<HostResponse> {
    let vars = json!({
        "expected": endpoint.host,
        "directory": directory,
        "days-ago": days_ago.to_string(),
        "show-untracked": show_untracked.to_string(),
        "tracked-names": serde_json::to_string(tracked_names)?,
    });
    let response: HostResponse =
        serde_json::from_value(endpoint.client.logs_eval(INVENTORY, &vars).await?)?;
    if response.host != endpoint.host
        || response.files.iter().any(|file| file.host != endpoint.host)
    {
        bail!("Host mismatch in log inventory response");
    }
    Ok(response)
}

#[cfg(test)]
async fn collect(endpoint: &Endpoint, directory: &str, requests: &[Value]) -> Result<HostResponse> {
    collect_since(endpoint, directory, requests, "").await
}

async fn collect_since(
    endpoint: &Endpoint,
    directory: &str,
    requests: &[Value],
    modified_since: &str,
) -> Result<HostResponse> {
    let response: HostResponse = serde_json::from_value(endpoint.client.logs_eval_download(COLLECT,
        &json!({"expected":endpoint.host, "directory":directory, "requests":serde_json::to_string(requests)?, "modified-since":modified_since})).await?)?;
    if response.host != endpoint.host
        || response.files.iter().any(|f| f.host != endpoint.host)
        || response.chunks.iter().any(|c| c.host != endpoint.host)
    {
        bail!("Host mismatch in log response");
    }
    Ok(response)
}

struct TimedScan {
    lines: Vec<String>,
    size: Option<u64>,
    modified: Option<String>,
}

async fn scan_records(
    client: &MarkLogicClient,
    host: &str,
    directory: &str,
    name: &str,
    start: &str,
) -> Result<TimedScan> {
    #[derive(Deserialize)]
    struct Scan {
        host: String,
        lines: Vec<String>,
        #[serde(default)]
        size: Option<u64>,
        #[serde(default)]
        modified: Option<String>,
    }
    let value = client
        .logs_eval(
            SCAN,
            &json!({"expected":host, "directory":directory, "name":name, "start":start, "limit":SCAN_LIMIT.to_string()}),
        )
        .await?;
    let scan: Scan = serde_json::from_value(value).context("Invalid log scan response")?;
    if scan.host != host {
        bail!("Host mismatch in log response");
    }
    Ok(TimedScan {
        lines: scan.lines,
        size: scan.size,
        modified: scan.modified,
    })
}

/// Read everything after the request's cursor, in bounded rounds. Each round
/// restarts at the last timestamp seen, so a long absence is drained without
/// a single huge response. A failure after progress keeps that progress.
async fn fetch_timed(
    client: &MarkLogicClient,
    directory: &str,
    request: TimedRequest,
) -> TimedChunk {
    let mut text = String::new();
    let mut cursor: Option<(String, usize)> = None;
    let mut start = request.start.clone();
    let mut skip = (request.skip_ts.clone(), request.skip_count);
    let mut size = None;
    let mut modified = None;
    let mut error = None;
    for _ in 0..SCAN_ROUNDS {
        let scan = match scan_records(client, &request.host, directory, &request.name, &start).await
        {
            Ok(scan) => scan,
            Err(e) => {
                error = Some(format!("{e:#}"));
                break;
            }
        };
        size = scan.size;
        modified = scan.modified;
        let mut lines = scan.lines;
        let truncated = lines.len() >= SCAN_LIMIT;
        if truncated
            && let Some(last) = lines.iter().rposition(|l| record_ts(l).is_some())
            && last > 0
        {
            // The final record may be cut off; the next round re-reads it.
            lines.truncate(last);
        }
        let round = dedupe_records(&lines.join("\n"), skip.0.as_deref(), skip.1);
        if let Some(ts) = round.last_ts {
            start = ts.replace(' ', "T");
            skip = (Some(ts.clone()), round.last_count);
            cursor = Some((ts, round.last_count));
        }
        let progressed = !round.text.is_empty();
        text.push_str(&round.text);
        if !truncated || !progressed {
            break;
        }
    }
    let (last_ts, last_count) = match cursor {
        Some((ts, count)) => (Some(ts), count),
        None => (None, 0),
    };
    let nothing_arrived = last_ts.is_none();
    TimedChunk {
        request,
        text,
        last_ts,
        last_count,
        size,
        modified,
        // Partial progress is committed; the error is reported only when
        // nothing arrived.
        error: error.filter(|_| nothing_arrived),
    }
}

fn is_error_log(name: &str) -> bool {
    let stem = name.strip_suffix(".txt").unwrap_or(name);
    let stem = match stem.rsplit_once('_') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => stem,
    };
    stem.ends_with("ErrorLog")
}

fn scope_start_for(now: NaiveDateTime, days: u32) -> String {
    let day: NaiveDate = now.date() - Days::days(i64::from(days.max(1)) - 1);
    format!("{}T00:00:00", day.format("%Y-%m-%d"))
}

fn timed_request(file: &LogFile, snapshot: Option<&Snapshot>, scope: &str) -> TimedRequest {
    let mut request = TimedRequest {
        host: file.host.clone(),
        path: file.path.clone(),
        name: file.name.clone(),
        start: scope.to_string(),
        skip_ts: None,
        skip_count: 0,
        reset: false,
    };
    match snapshot {
        // Byte-offset cache from an older version: start a time-addressed
        // generation, keeping the old one as *.previous.log.
        Some(snapshot) if snapshot.time.is_none() => request.reset = true,
        Some(snapshot) => {
            let time = snapshot.time.as_ref().expect("checked above");
            if time.since.as_str() != scope {
                // A history-window change replaces the generation. A narrow
                // window must never resume an older cursor and pull its gap.
                request.reset = true;
            } else {
                // Resume only inside the same selected window.
                let cursor = time.ts.replace(' ', "T");
                if cursor.as_str() >= scope {
                    request.start = cursor;
                    request.skip_ts = Some(time.ts.clone());
                    request.skip_count = time.count;
                } else {
                    request.reset = true;
                }
            }
        }
        None => {}
    }
    request
}

struct Deduped {
    text: String,
    last_ts: Option<String>,
    last_count: usize,
}

/// The leading `YYYY-MM-DD HH:MM:SS.mmm` of a record's first line.
fn record_ts(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    if bytes.len() <= TS_LEN || bytes[TS_LEN] != b' ' {
        return None;
    }
    let shape = b"dddd-dd-dd dd:dd:dd.ddd";
    for (byte, want) in bytes.iter().zip(shape) {
        let ok = if *want == b'd' {
            byte.is_ascii_digit()
        } else {
            byte == want
        };
        if !ok {
            return None;
        }
    }
    line.get(..TS_LEN)
}

/// Preserve complete error-log records at or after the local server-time
/// window. A continuation belongs to the record directly before it.
fn error_text_since(text: &str, scope: &str) -> String {
    // Older cache formats and hand-written/partial fixtures may not carry a
    // recognizable MarkLogic timestamp. Retain those verbatim: there is no
    // safe boundary to apply locally, and the next timestamped fetch replaces
    // the legacy generation.
    if !text.lines().any(|line| record_ts(line).is_some()) {
        return text.to_owned();
    }
    let scope = scope.replace('T', " ");
    let mut keeping = false;
    let mut output = String::new();
    for line in text.split_inclusive('\n') {
        if let Some(timestamp) = record_ts(line) {
            keeping = timestamp >= scope.as_str();
        }
        if keeping {
            output.push_str(line);
        }
    }
    output
}

/// Keep only records not already held. The request window starts inclusively
/// at the last held timestamp, so the first `skip_count` records carrying it
/// are the ones already cached. Continuation lines follow their record.
fn dedupe_records(raw: &str, skip_ts: Option<&str>, skip_count: usize) -> Deduped {
    let mut out = String::new();
    let mut last_ts: Option<String> = None;
    let mut last_count = 0;
    let mut skipped = 0;
    let mut keep = skip_ts.is_none();
    let mut seen_record = false;
    for line in raw.lines() {
        if let Some(ts) = record_ts(line) {
            seen_record = true;
            if last_ts.as_deref() == Some(ts) {
                last_count += 1;
            } else {
                last_ts = Some(ts.to_string());
                last_count = 1;
            }
            keep = match skip_ts {
                Some(held) if ts < held => false,
                Some(held) if ts == held && skipped < skip_count => {
                    skipped += 1;
                    false
                }
                _ => true,
            };
        } else if !seen_record {
            // Lines before the first record belong to a record we cannot see.
            keep = skip_ts.is_none();
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    Deduped {
        text: out,
        last_ts,
        last_count,
    }
}

fn validate_chunk(file: &LogFile, chunk: &Chunk) -> Result<()> {
    if chunk.end < chunk.start
        || chunk.end > chunk.length
        || chunk.text.chars().count() as u64 != chunk.end - chunk.start
    {
        bail!("Invalid log chunk range for {}", file.path);
    }
    Ok(())
}

#[cfg(test)]
fn merge_chunk(file: LogFile, previous: Option<&Snapshot>, chunk: &Chunk) -> Result<Snapshot> {
    validate_chunk(&file, chunk)?;
    let mut snapshot = match previous {
        Some(previous) if !chunk.reset => previous.clone(),
        _ => Snapshot {
            file: file.clone(),
            start: chunk.start,
            end: chunk.start,
            length: chunk.length,
            anchor_start: 0,
            anchor: String::new(),
            text: String::new(),
            text_bytes: 0,
            generation: previous.map(|s| s.generation + 1).unwrap_or(0),
            time: None,
            full_size: None,
            updated_at: None,
        },
    };
    if snapshot.end != chunk.start {
        bail!("Non-contiguous log chunk; refresh before retrying");
    }
    snapshot.text.push_str(&chunk.text);
    snapshot.file = file;
    snapshot.end = chunk.end;
    snapshot.length = chunk.length;
    snapshot.anchor_start = chunk.anchor_start;
    snapshot.anchor = chunk.anchor.clone();
    Ok(snapshot)
}

fn take_delta(counter: &AtomicU64, accounted: &mut u64) -> u64 {
    let total = counter.load(Ordering::Relaxed);
    let delta = total - *accounted;
    *accounted = total;
    delta
}

fn format_cache_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds < 60 {
        "<1m".into()
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else if seconds < 24 * 60 * 60 {
        format!("{}h", seconds / (60 * 60))
    } else {
        format!("{}d", seconds / (24 * 60 * 60))
    }
}

pub(crate) fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{AuthType, ServerConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    static TEST_DIRECTORY_ID: AtomicUsize = AtomicUsize::new(0);

    fn file(host: &str) -> LogFile {
        LogFile {
            host: host.into(),
            path: "/Logs/ErrorLog.txt".into(),
            name: "ErrorLog.txt".into(),
            size: 20,
            modified: "2026-10-14T12:00:00Z".into(),
            present: true,
        }
    }
    fn chunk(host: &str, start: u64, text: &str, reset: bool) -> Chunk {
        let end = start + text.chars().count() as u64;
        Chunk {
            host: host.into(),
            path: file(host).path,
            start,
            end,
            length: end,
            reset,
            text: text.into(),
            anchor_start: start,
            anchor: text.into(),
            error: None,
            full: false,
        }
    }
    fn response(host: &str, chunks: Vec<Chunk>) -> HostResponse {
        HostResponse {
            host: host.into(),
            directory: "/Logs".into(),
            inventory: false,
            read_bytes: 20,
            cluster_bytes: 0,
            files: vec![file(host)],
            chunks,
        }
    }
    fn viewer() -> LogViewer {
        let path = std::env::temp_dir().join(format!(
            "marklogic-tui-logs-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).unwrap();
        LogViewer {
            cache_root: path,
            clock: Some((
                NaiveDate::from_ymd_opt(2026, 10, 14)
                    .unwrap()
                    .and_hms_opt(9, 0, 0)
                    .unwrap(),
                Instant::now(),
            )),
            ..LogViewer::default()
        }
    }

    fn test_client() -> MarkLogicClient {
        MarkLogicClient::new(ServerConfig {
            name: "log-test".into(),
            host: "127.0.0.1".into(),
            username: "tester".into(),
            password: "secret".into(),
            port: 8000,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: Vec::new(),
            uri: None,
        })
        .unwrap()
    }

    #[test]
    fn opening_an_empty_cache_starts_discovery_without_showing_untracked() {
        let mut logs = viewer();
        let client = test_client();
        logs.scope_days = 30;
        logs.identity = format!(
            "{}\0{}\0{}\0",
            client.server.name,
            client.server.base_url(),
            client.server.username,
        );
        let runtime = Runtime::new().unwrap();

        logs.open(Some(client), "", crate::AppMode::Normal, &runtime);

        assert!(logs.busy);
        assert!(logs.endpoints.is_empty());
        assert_eq!(logs.scope_days, 1);
        assert!(!logs.show_untracked);
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn opening_cached_logs_checks_every_discovered_host_even_after_a_failure() {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move {
            let mut checked = Vec::new();
            for index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(stream.read_u8().await.unwrap());
                }
                let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| {
                    line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)
                }).unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                let (status, response) = if index == 0 {
                    assert_eq!(form["xquery"], DISCOVER);
                    ("200 OK", json!({"now":"2026-10-14T09:00:00Z", "hosts": [
                        "d-empty", "c-data", "a-default", "b-offline", "a-default"
                    ]}).to_string())
                } else {
                    assert_eq!(form["xquery"], INVENTORY);
                    let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                    assert_eq!(vars["show-untracked"], "false");
                    let names: Vec<String> = serde_json::from_str(vars["tracked-names"].as_str().unwrap()).unwrap();
                    assert!(names.contains(&"ErrorLog.txt".to_string()));
                    let host = vars["expected"].as_str().unwrap();
                    checked.push(host.to_string());
                    if host == "b-offline" {
                        ("500 Internal Server Error", "host unavailable".into())
                    } else {
                        let files = if host == "d-empty" { vec![] } else { vec![json!({
                            "host": host, "path":"/Logs/ErrorLog.txt", "name":"ErrorLog.txt",
                            "size":20, "modified":"2026-10-14T09:00:00Z"
                        })] };
                        ("200 OK", json!({"host":host, "directory":"/Logs", "inventory":false,
                            "read_bytes":0, "cluster_bytes":0, "files":files, "chunks":[]}).to_string())
                    }
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
            checked
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        logs.identity = format!(
            "{}\0{}\0{}\0",
            client.server.name,
            client.server.base_url(),
            client.server.username
        );
        logs.files.push(file("a-default"));
        logs.open(Some(client), "", crate::AppMode::Normal, &runtime);
        assert!(logs.busy, "a cached host must not bypass cluster discovery");
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while logs.busy {
                    logs.tick(&runtime, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                assert_eq!(
                    server.await.unwrap(),
                    ["a-default", "b-offline", "c-data", "d-empty"]
                );
            })
            .await
            .unwrap();
        });
        assert_eq!(logs.endpoints.len(), 4);
        assert!(logs.files.iter().any(|file| file.host == "c-data"));
        assert!(logs.errors["b-offline"].contains("host unavailable"));
        assert_eq!(
            logs.host_legend_lines(),
            ["h1=a-default", "h2=b-offline", "h3=c-data", "h4=d-empty"]
        );
        assert_eq!(logs.host_alias("c-data"), "h3");
        assert!(!logs.show_untracked);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    fn named_file(host: &str, name: &str, modified: &str) -> LogFile {
        LogFile {
            host: host.into(),
            path: format!("/Logs/{name}"),
            name: name.into(),
            size: 20,
            modified: modified.into(),
            present: true,
        }
    }

    #[test]
    fn only_three_streams_are_tracked_by_default_and_the_rest_are_opt_in() {
        let mut logs = viewer();
        let names = [
            "ErrorLog.txt",
            "TaskServer_ErrorLog.txt",
            "8000_ErrorLog.txt",
            "8000_AccessLog.txt",
            "8040_ErrorLog.txt",
            "ErrorLog_1.txt",
            "AuditLog.txt",
        ];
        let tracked = |logs: &LogViewer| -> Vec<&str> {
            names
                .into_iter()
                .filter(|n| logs.is_subscribed(&named_file("one", n, "2026-10-14T00:00:00Z")))
                .collect()
        };
        assert_eq!(
            tracked(&logs),
            [
                "ErrorLog.txt",
                "TaskServer_ErrorLog.txt",
                "8000_ErrorLog.txt"
            ]
        );
        logs.extra_streams.insert("8040_ErrorLog.txt".into());
        logs.disabled_streams.insert("ErrorLog.txt".into());
        assert!(tracked(&logs).contains(&"8040_ErrorLog.txt"));
        assert!(!tracked(&logs).contains(&"ErrorLog.txt"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn catalog_and_modes_survive_without_downloaded_content() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.modes.insert("ErrorLog.txt".into(), TrackingMode::Live);
        logs.save_subscriptions();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(restored.files.len(), 1);
        assert_eq!(
            restored.tracking_mode(&restored.files[0]),
            TrackingMode::Live
        );
        assert_eq!(restored.visible_file_indices(), vec![0]);
        assert!(restored.snapshots.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn cycling_modes_never_untracks_and_explicit_untrack_keeps_cache() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "abc", true)]))
            .unwrap();
        logs.apply(response("two", vec![])).unwrap();
        let runtime = Runtime::new().unwrap();
        for expected in [
            TrackingMode::Live,
            TrackingMode::Partial,
            TrackingMode::OnDemand,
            TrackingMode::Live,
        ] {
            logs.toggle_subscription(&runtime);
            assert_eq!(logs.visible_file_indices().len(), 1);
            assert!(
                logs.files
                    .iter()
                    .all(|file| logs.tracking_mode(file) == expected)
            );
        }
        logs.untrack_selected();
        assert!(logs.visible_file_indices().is_empty());
        assert!(
            logs.files
                .iter()
                .all(|file| logs.tracking_mode(file) == TrackingMode::Untracked)
        );
        assert_eq!(logs.snapshots[&file("one").key()].text, "abc");
        assert!(logs.message.contains("cached content kept"));
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(
            restored.tracking_mode(&file("one")),
            TrackingMode::Untracked
        );
        assert_eq!(restored.snapshots[&file("one").key()].text, "abc");
        logs.show_untracked = true;
        assert_eq!(logs.visible_file_indices().len(), 1);
        logs.toggle_subscription(&runtime);
        assert!(
            logs.files
                .iter()
                .all(|file| logs.tracking_mode(file) == TrackingMode::OnDemand)
        );
        assert_eq!(logs.snapshots[&file("one").key()].text, "abc");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_bounds_apply_before_search_context_file_switch_merge_and_export() {
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: outside-before\n2026-10-14 08:05:00.000 Error: first-inside\n2026-10-14 08:05:00.000 Error+: detail-inside\n2026-10-14 08:10:00.000 Info: last-inside\n2026-10-14 08:15:00.000 Info: outside-after\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.files_focus = true;
        logs.scroll = 1;
        logs.set_truncation_at_cursor(true);
        assert_eq!(logs.window, TimeWindow::default());
        logs.files_focus = false;
        logs.view_height = 1;
        logs.scroll = 1;
        logs.set_truncation_at_cursor(true);
        assert_eq!(
            logs.window.start.unwrap().format("%H:%M").to_string(),
            "08:05"
        );
        assert!(!logs.visible_text().contains("outside-before"));
        let end_position = logs
            .matches
            .iter()
            .position(|&row| logs.display_row(row).to_string().contains("last-inside"))
            .unwrap();
        logs.host_anchor_position = Some(end_position);
        logs.set_truncation_at_cursor(false);
        assert_eq!(
            logs.window.end.unwrap().format("%H:%M").to_string(),
            "08:10"
        );
        assert!(logs.visible_text().contains("detail-inside"));
        assert!(!logs.visible_text().contains("outside-after"));
        let export = logs.editor_export().unwrap();
        let exported = fs::read_to_string(&export).unwrap();
        assert!(exported.contains("first-inside"));
        assert!(!exported.contains("outside-before") && !exported.contains("outside-after"));
        fs::remove_file(export).unwrap();
        logs.context_before = 10;
        logs.context_after = 10;
        logs.search = "outside".into();
        logs.filter();
        assert!(logs.matches.is_empty());
        logs.search.clear();
        logs.filter();
        let other = named_file("one", "TaskServer_ErrorLog.txt", "2026-10-14T08:15:00Z");
        let other_text = "2026-10-14 08:04:00.000 Info: other-before\n2026-10-14 08:05:00.000 Info: other-inside\n2026-10-14 08:11:00.000 Info: other-after\n";
        logs.files.push(other.clone());
        logs.commit(other, true, other_text, |_| {}).unwrap();
        let index = logs
            .files
            .iter()
            .position(|file| file.name == "TaskServer_ErrorLog.txt")
            .unwrap();
        logs.select(index);
        assert!(logs.visible_text().contains("other-inside"));
        assert!(
            !logs.visible_text().contains("other-before")
                && !logs.visible_text().contains("other-after")
        );
        logs.toggle_all_cached();
        let raw = logs.visible_text();
        assert!(raw.contains("first-inside") && raw.contains("other-inside"));
        assert!(!raw.contains("outside-before") && !raw.contains("other-after"));
        logs.toggle_combined();
        let merged = logs.visible_text();
        assert!(merged.contains("first-inside") && merged.contains("other-inside"));
        assert!(!merged.contains("outside-before") && !merged.contains("other-after"));
        logs.search = "outside".into();
        logs.filter();
        assert!(logs.matches.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_keys_select_any_visible_line_and_dialog_is_rendered() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:05:00.000 Info: middle\n2026-10-14 08:10:00.000 Info: last\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.files_focus = false;
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        app.server_logs.move_host_anchor_to_edge(false);
        for _ in 0..2 {
            crate::events::handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
            )
            .unwrap();
        }
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let y = (0..30)
            .find(|&y| {
                (0..120)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .contains("last")
            })
            .unwrap();
        assert!((0..120).any(|x| buffer[(x, y)].bg == ratatui::style::Color::DarkGray));
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT),
        )
        .unwrap();
        assert_eq!(
            app.server_logs
                .window
                .end
                .unwrap()
                .format("%H:%M")
                .to_string(),
            "08:10"
        );
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
        )
        .unwrap();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            app.server_logs
                .window
                .start
                .unwrap()
                .format("%H:%M")
                .to_string(),
            "08:00"
        );
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        )
        .unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let contents: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(contents.contains("Truncation"));
        assert!(contents.contains("Adjustment in minutes"));
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.mode, crate::AppMode::MarkLogicLogs);
        assert!(app.server_logs.truncation_dialog.as_ref().unwrap().fields[0].ends_with('?'));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_can_start_from_an_all_cached_raw_line() {
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:05:00.000 Info: second\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.toggle_all_cached();
        logs.files_focus = false;
        logs.move_host_anchor_to_edge(true);
        logs.set_truncation_at_cursor(true);
        assert_eq!(
            logs.window.start.unwrap().format("%H:%M").to_string(),
            "08:05"
        );
        assert!(logs.visible_text().contains("second"));
        assert!(!logs.visible_text().contains("first"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_applies_to_new_and_out_of_order_log_records() {
        let mut logs = viewer();
        let text = "2026-10-14 08:05:00.000 Info: inside\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        let offset = FixedOffset::east_opt(0).unwrap();
        logs.apply_window(
            TimeWindow {
                start: presentation::parse_time("2026-10-14 08:00:00", offset),
                end: presentation::parse_time("2026-10-14 08:10:00", offset),
            },
            None,
        );
        let extra =
            "2026-10-14 08:20:00.000 Info: outside\n2026-10-14 08:07:00.000 Info: late-inside\n";
        logs.apply(response(
            "one",
            vec![chunk("one", text.chars().count() as u64, extra, false)],
        ))
        .unwrap();
        let visible = logs.visible_text();
        assert!(visible.contains("inside") && visible.contains("late-inside"));
        assert!(!visible.contains("outside"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn truncate_dialog_validates_adjusts_cancels_and_resets_bounds() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        app.server_logs.open_truncation_dialog();
        let dialog = app.server_logs.truncation_dialog.as_mut().unwrap();
        dialog.fields = [
            "2026-10-14 08:00:00".into(),
            "2026-10-14 09:00:00".into(),
            "15".into(),
        ];
        app.server_logs.adjust_truncation_dialog(true);
        assert!(app.server_logs.truncation_dialog.as_ref().unwrap().fields[0].contains("08:15"));
        app.server_logs.apply_truncation_dialog();
        assert_eq!(
            app.server_logs
                .window
                .start
                .unwrap()
                .format("%H:%M")
                .to_string(),
            "08:15"
        );
        app.server_logs.open_truncation_dialog();
        app.server_logs.truncation_dialog.as_mut().unwrap().fields[1] =
            "2026-10-14 07:00:00".into();
        app.server_logs.apply_truncation_dialog();
        assert!(
            app.server_logs
                .truncation_dialog
                .as_ref()
                .unwrap()
                .error
                .contains("Start")
        );
        assert_eq!(
            app.server_logs
                .window
                .end
                .unwrap()
                .format("%H:%M")
                .to_string(),
            "09:00"
        );
        crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.server_logs.truncation_dialog.is_none());
        assert!(app.server_logs.window.active());
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        )
        .unwrap();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        )
        .unwrap();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(app.server_logs.window, TimeWindow::default());
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_uses_highlighted_host_anchor_and_excludes_untimed_records() {
        let mut logs = viewer();
        let one = "untimed preface\n2026-10-14 08:00:00.000 Info: before\n2026-10-14 08:05:00.000 Info: chosen\n2026-10-14 08:10:00.000 Info: after\n";
        let two = "2026-10-14 08:01:00.000 Info: other-before\n2026-10-14 08:06:00.000 Info: other-after\n";
        logs.apply(response("one", vec![chunk("one", 0, one, true)]))
            .unwrap();
        logs.apply(response("two", vec![chunk("two", 0, two, true)]))
            .unwrap();
        logs.files_focus = false;
        logs.view_height = 3;
        logs.scroll = 0;
        logs.host_anchor_position = Some(2);
        logs.set_truncation_at_cursor(true);
        assert_eq!(
            logs.window.start.unwrap().format("%H:%M").to_string(),
            "08:05"
        );
        assert!(!logs.visible_text().contains("untimed preface"));
        let other = logs
            .host_views
            .iter()
            .find(|view| view.host == "two")
            .unwrap();
        assert_eq!(other.rows.len(), 1);
        assert!(
            logs.display_host_row(other, 0)
                .to_string()
                .contains("other-after")
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn large_logs_format_only_the_viewport_and_search_the_whole_file() {
        let mut logs = viewer();
        let text: String = (0..5000)
            .map(|i| format!("2026-10-14 08:00:00.000 Info: entry-{i:05}\n"))
            .collect();
        logs.apply(response("one", vec![chunk("one", 0, &text, true)]))
            .unwrap();
        assert_eq!(logs.matches.len(), 5000);
        assert!(
            logs.rendered_records.borrow().is_empty(),
            "opening must not format every record"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        assert!(logs.rendered_records.borrow().len() <= 64);
        assert!(logs.display_row(4999).to_string().contains("entry-04999"));
        logs.scroll = 0;
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        assert!(logs.display_row(0).to_string().contains("entry-00000"));
        assert!(logs.rendered_records.borrow().len() <= 3 * logs.view_height.max(1));
        logs.search = "entry-00001".into();
        logs.filter();
        assert_eq!(logs.matches.len(), 1);
        assert!(logs.visible_text().contains("entry-00001"));
        logs.search.clear();
        logs.filter();
        logs.toggle_all_cached();
        assert!(logs.rendered_records.borrow().is_empty());
        assert!(logs.display_row(4999).to_string().contains("entry-04999"));
        logs.search = "entry-00001".into();
        logs.filter();
        assert_eq!(logs.matches.len(), 1);
        assert!(logs.visible_text().contains("entry-00001"));
        logs.search.clear();
        logs.toggle_all_cached();
        logs.toggle_combined();
        assert!(logs.rendered_records.borrow().is_empty());
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        assert!(logs.rendered_records.borrow().len() <= 3 * logs.view_height.max(1));
        logs.search = "entry-00001".into();
        logs.filter();
        assert!(logs.visible_text().contains("entry-00001"));
        assert!(!logs.visible_text().contains("entry-04999"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn server_clock_offset_change_rebuilds_lazy_file_records() {
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: preserved record\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        assert!(logs.display_row(0).to_string().contains("preserved record"));
        logs.tx
            .send(CollectorEvent::Clock("2026-10-14T10:00:00+02:00".into()))
            .unwrap();
        let rt = Runtime::new().unwrap();
        logs.tick(&rt, false);
        assert!(logs.display_row(0).to_string().contains("preserved record"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn wrapped_combined_view_materializes_the_correct_wrapped_lines() {
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: abcdefghijklmnopqrstuvwxyz\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.options.wrap = true;
        logs.view_width = 12;
        logs.toggle_combined();
        assert!(logs.rendered_records.borrow().is_empty());
        let text = logs.visible_text().replace('\n', "");
        assert!(text.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(logs.rows.len() > 2);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn grouped_host_views_align_to_leading_time_and_exclude_empty_hosts() {
        let mut logs = viewer();
        let one = "2026-10-14 10:00:00.000 Info: one\n2026-10-14 10:01:00.000 Error: two\n2026-10-14 10:02:00.000 Info: three\n";
        let two = "2026-10-14 10:00:10.000 Info: alpha\n2026-10-14 10:01:10.000 Error: beta\n2026-10-14 10:03:00.000 Info: gamma\n";
        logs.apply(response("one", vec![chunk("one", 0, one, true)]))
            .unwrap();
        logs.apply(response("two", vec![chunk("two", 0, two, true)]))
            .unwrap();
        let mut empty = response("three", vec![]);
        empty.files[0].size = 0;
        logs.apply(empty).unwrap();
        assert_eq!(logs.visible_file_indices().len(), 1);
        assert_eq!(logs.group_summaries(Utc::now())["ErrorLog.txt"].0, 3);
        logs.file_filter = "two".into();
        assert_eq!(logs.visible_file_indices().len(), 1);
        assert_eq!(logs.host_views.len(), 2);
        logs.file_filter.clear();
        let requests = logs.on_demand_requests(Some("ErrorLog.txt"));
        assert_eq!(
            requests.keys().map(String::as_str).collect::<Vec<_>>(),
            ["one", "three", "two"]
        );
        assert_eq!(logs.host_views.len(), 2);
        assert_eq!(
            logs.host_views
                .iter()
                .map(|view| view.host.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        logs.view_height = 1;
        logs.scroll = 1;
        let target = logs.anchor_timestamp().unwrap();
        let other = &logs.host_views[1];
        let nearest = logs.host_anchor(other).unwrap();
        assert_eq!(
            other.rows[other.matches[nearest]].timestamp.unwrap(),
            target + Days::seconds(10)
        );
        logs.cycle_leading_host(false);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        assert_eq!(logs.anchor_timestamp().unwrap(), target + Days::seconds(10));
        logs.search = "error".into();
        logs.filter();
        assert_eq!(
            logs.host_views
                .iter()
                .map(|view| view.matches.len())
                .collect::<Vec<_>>(),
            [1, 1]
        );
        logs.search.clear();
        logs.filter();
        logs.scroll_to_bottom();
        logs.files_focus = false;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 36)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let content_rows: Vec<String> = (0..36)
            .map(|y| (50..110).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(content_rows.iter().any(|row| row.contains("one")));
        assert!(content_rows.iter().any(|row| row.contains("two · leading")));
        let lead_row = content_rows
            .iter()
            .position(|row| row.contains("two · leading"))
            .unwrap() as u16;
        assert_eq!(buffer[(50, lead_row)].fg, ratatui::style::Color::Yellow);
        assert!(!content_rows.iter().any(|row| row.starts_with("┌three")));
        for message in ["three", "gamma"] {
            let y = content_rows
                .iter()
                .position(|row| row.contains(message))
                .unwrap() as u16;
            assert!((50..110).any(|x| buffer[(x, y)].bg == ratatui::style::Color::DarkGray));
        }
        logs.view_height = 3;
        logs.files_focus = true;
        logs.host_anchor_position = Some(0);
        logs.scroll = 0;
        logs.cycle_focus(false);
        assert!(!logs.files_focus);
        assert_eq!(logs.selected_file().unwrap().host, "one");
        assert_eq!(logs.anchor_position(), Some(0));
        logs.cycle_focus(false);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        assert_eq!(logs.anchor_position(), Some(0));
        logs.cycle_focus(false);
        assert!(logs.files_focus);
        logs.cycle_focus(true);
        assert!(!logs.files_focus);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        logs.cycle_focus(true);
        assert_eq!(logs.selected_file().unwrap().host, "one");
        logs.cycle_focus(true);
        assert!(logs.files_focus);
        logs.toggle_sidebar();
        logs.cycle_focus(true);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        logs.cycle_focus(false);
        assert_eq!(logs.selected_file().unwrap().host, "one");
        assert!(logs.move_host_anchor(1));
        assert_eq!(logs.anchor_position(), Some(1));
        assert_eq!(logs.host_anchor(&logs.host_views[1]), Some(1));
        assert!(logs.move_host_anchor(-3));
        assert_eq!(logs.anchor_position(), Some(0));
        assert_eq!(logs.host_anchor(&logs.host_views[1]), Some(0));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn mouse_and_tab_select_the_host_panes() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{Terminal, backend::TestBackend};
        let mut logs = viewer();
        for host in ["one", "two"] {
            let line = format!("2026-10-14 10:00:00.000 Info: {host}\n");
            logs.apply(response(host, vec![chunk(host, 0, &line, true)]))
                .unwrap();
        }
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let mut terminal = Terminal::new(TestBackend::new(110, 36)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let (host_area, _) = app
            .server_logs
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, LogMouseTarget::Host(1)))
            .copied()
            .unwrap();
        crate::events::handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: host_area.x + 1,
                row: host_area.y,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(!app.server_logs.files_focus);
        assert_eq!(app.server_logs.selected_file().unwrap().host, "two");
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let (files_area, _) = app
            .server_logs
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, LogMouseTarget::Files { .. }))
            .copied()
            .unwrap();
        crate::events::handle_terminal_event(
            &mut app,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: files_area.x + 1,
                row: files_area.y,
                modifiers: KeyModifiers::NONE,
            }),
        )
        .unwrap();
        assert!(app.server_logs.files_focus);
        crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.server_logs.selected_file().unwrap().host, "one");
        crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.server_logs.selected_file().unwrap().host, "two");
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        )
        .unwrap();
        assert_eq!(app.server_logs.selected_file().unwrap().host, "one");
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn cache_age_uses_content_updates_and_survives_restart() {
        let mut logs = viewer();
        let source = file("one");
        let now = Utc::now();
        assert_eq!(logs.cache_age(&source, now), "n/a");
        logs.apply(response("one", vec![chunk("one", 0, "abc", true)]))
            .unwrap();
        let saved = logs.snapshots[&source.key()].updated_at.clone().unwrap();
        assert_eq!(logs.cache_age(&source, Utc::now()), "<1m");
        logs.update_timed_file_metadata(
            "one",
            &source.path,
            Some(20),
            Some("2026-10-14T12:00:00Z"),
        )
        .unwrap();
        assert_eq!(
            logs.snapshots[&source.key()].updated_at.as_deref(),
            Some(saved.as_str())
        );
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(
            restored.snapshots[&source.key()].updated_at.as_deref(),
            Some(saved.as_str())
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn selected_file_shows_each_hosts_size_and_cache_age() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "abc", true)]))
            .unwrap();
        logs.snapshots
            .get_mut(&file("one").key())
            .unwrap()
            .updated_at = Some((Utc::now() - Days::hours(2)).to_rfc3339());
        let mut second = file("two");
        second.size = 2048;
        logs.files.push(second);
        let mut other = named_file("one", "8000_ErrorLog.txt", "2026-10-14T12:00:00Z");
        other.size = 17;
        logs.files.push(other);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let lines: Vec<String> = (0..24)
            .map(|y| {
                (0..120)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let row = lines
            .iter()
            .position(|line| line.contains("h1") && line.contains("20 B  2h"))
            .unwrap();
        let group = lines
            .iter()
            .position(|line| line.contains("ErrorLog.txt (2 hosts)"))
            .unwrap();
        assert_eq!(row, group + 1);
        assert!(lines[group + 2].contains("h2") && lines[group + 2].contains("2.0 KiB  n/a"));
        assert!(lines[group + 3].contains("8000_ErrorLog.txt"));
        assert!(lines.iter().any(|line| line.contains("h1=one")));
        assert!(lines.iter().any(|line| line.contains("h2=two")));
        let age_column = lines[row].split("2h").next().unwrap().chars().count();
        assert_eq!(
            terminal.backend().buffer()[(age_column as u16, row as u16)].fg,
            ratatui::style::Color::Cyan
        );

        let other = logs
            .files
            .iter()
            .position(|file| file.name == "8000_ErrorLog.txt")
            .unwrap();
        logs.select(other);
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let lines: Vec<String> = (0..24)
            .map(|y| {
                (0..120)
                    .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                    .collect()
            })
            .collect();
        let group = lines
            .iter()
            .position(|line| line.contains("8000_ErrorLog.txt (1 host)"))
            .unwrap();
        assert!(lines[group + 1].contains("h1") && lines[group + 1].contains("17 B  n/a"));
        assert!(lines[group + 2].contains("h2") && lines[group + 2].contains("not listed  n/a"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn clicking_expanded_host_rows_and_following_file_keeps_selection_correct() {
        use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        app.server_logs.files = vec![
            file("one"),
            file("two"),
            named_file("one", "8000_ErrorLog.txt", "2026-10-14T12:00:00Z"),
        ];
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let (area, target) = app
            .server_logs
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, LogMouseTarget::Files { .. }))
            .copied()
            .unwrap();
        let LogMouseTarget::Files {
            offset,
            selected: Some(selected),
            details,
        } = target
        else {
            panic!("missing selected file")
        };
        assert_eq!(details, 2);
        for (row, expected) in [
            (selected + 1, "ErrorLog.txt"),
            (selected + details + 1, "8000_ErrorLog.txt"),
        ] {
            crate::events::handle_terminal_event(
                &mut app,
                Event::Mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x + 2,
                    row: area.y + 1 + (row - offset) as u16,
                    modifiers: KeyModifiers::NONE,
                }),
            )
            .unwrap();
            assert_eq!(app.server_logs.selected_file().unwrap().name, expected);
        }
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn cache_age_formats_minutes_hours_days_and_future_times() {
        for (seconds, expected) in [
            (-10, "<1m"),
            (59, "<1m"),
            (60, "1m"),
            (34 * 60, "34m"),
            (3600, "1h"),
            (2 * 86400, "2d"),
        ] {
            assert_eq!(format_cache_age(seconds), expected);
        }
    }

    #[test]
    fn only_complete_downloads_record_a_full_size() {
        let mut logs = viewer();
        let mut partial = response("one", vec![chunk("one", 0, "abc", true)]);
        partial.files[0].size = 3;
        logs.apply(partial).unwrap();
        let key = file("one").key();
        assert_eq!(logs.snapshots[&key].full_size, None);

        let mut complete = response("one", vec![chunk("one", 0, "abc", true)]);
        complete.files[0].size = 3;
        complete.chunks[0].full = true;
        logs.apply(complete).unwrap();
        assert_eq!(logs.snapshots[&key].full_size, Some(3));

        let mut changed = response("one", vec![chunk("one", 0, "abcd", true)]);
        changed.files[0].size = 3;
        changed.chunks[0].full = true;
        logs.apply(changed).unwrap();
        assert_eq!(logs.snapshots[&key].full_size, None);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn sidebar_sorts_tracked_streams_before_host_port_and_log_kind() {
        let mut logs = viewer();
        logs.files = vec![
            named_file("a-host", "9060_AccessLog.txt", "2026-10-14T00:00:00Z"),
            named_file("a-host", "ErrorLog.txt", "2026-10-14T00:00:00Z"),
            named_file("a-host", "9060_ErrorLog.txt", "2026-10-14T00:00:00Z"),
            named_file("z-host", "ErrorLog.txt", "2026-10-14T00:00:00Z"),
            named_file("b-host", "8040_ErrorLog.txt", "2026-10-14T00:00:00Z"),
        ];

        logs.sort_files();

        assert_eq!(
            logs.files
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            [
                "ErrorLog.txt",
                "ErrorLog.txt",
                "9060_ErrorLog.txt",
                "9060_AccessLog.txt",
                "8040_ErrorLog.txt",
            ]
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn context_includes_surrounding_lines_without_duplicates() {
        let mut logs = viewer();
        logs.lines = ["zero", "match", "two", "match", "four"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        logs.search = "match".into();
        logs.context_before = 1;
        logs.context_after = 1;
        logs.filter();
        assert_eq!(logs.matches, vec![0, 1, 2, 3, 4]);
        logs.context_before = 0;
        logs.context_after = 0;
        logs.filter();
        assert_eq!(logs.matches, vec![1, 3]);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn scroll_to_bottom_accounts_for_the_content_pane_height() {
        let mut logs = viewer();
        logs.matches = (0..25).collect();
        logs.view_height = 8;

        logs.scroll_to_bottom();

        assert_eq!(logs.scroll, 17);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn combined_view_merges_tracked_records_and_filters_whole_blocks() {
        let mut logs = viewer();
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 14)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        let sources = [
            (
                named_file("one", "ErrorLog.txt", "2026-10-14T01:00:00Z"),
                "2026-10-13 23:59:00.000 Info: yesterday\n2026-10-14 00:02:00.000 Error: failure\n2026-10-14 00:02:00.000 Error+: special detail\n",
            ),
            (
                named_file("two", "8000_ErrorLog.txt", "2026-10-14T01:00:00Z"),
                "2026-10-14 00:01:00.000 Info: earlier\n",
            ),
            (
                named_file("one", "9060_AccessLog.txt", "2026-10-14T01:00:00Z"),
                "127.0.0.1 - admin [14/Oct/2026:00:03:00 +0000] \"GET /v1/documents HTTP/1.1\" 200 42\n",
            ),
        ];
        for (file, text) in sources {
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.toggle_combined();
        let text = logs.visible_text();
        assert!(text.find("earlier").unwrap() < text.find("failure").unwrap());
        assert!(!text.contains("yesterday"));
        assert!(!text.contains("GET"), "untracked cached streams are opt-in");

        logs.search = "special".into();
        logs.filter();
        let text = logs.visible_text();
        assert!(text.contains("failure"));
        assert!(text.contains("special detail"));
        assert!(!text.contains("earlier"));
        logs.scroll = 0;
        logs.jump_record(false, false);
        assert!(logs.rows[logs.matches[logs.scroll]].start);

        logs.search.clear();
        logs.extra_streams.insert("9060_AccessLog.txt".into());
        logs.rebuild_lines();
        assert!(logs.visible_text().contains("GET"));
        logs.port_filters.insert("9060".into(), PortFilter::Include);
        logs.include_only_marked_ports = true;
        logs.rebuild_lines();
        let text = logs.visible_text();
        assert!(text.contains("GET"));
        assert!(!text.contains("failure"));
        logs.port_filters.insert("9060".into(), PortFilter::Exclude);
        logs.rebuild_lines();
        assert!(logs.matches.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn combined_follow_renders_a_batch_and_attaches_a_late_continuation() {
        let mut logs = viewer();
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 14)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        logs.apply(response("one", vec![])).unwrap();
        logs.toggle_combined();
        logs.view_height = 2;
        let rt = Runtime::new().unwrap();
        let messages = [
            "2026-10-14 00:01:00.000 Error: start\n",
            "2026-10-14 00:01:00.000 Error+: delayed stack\n2026-10-14 00:02:00.000 Info: end\n",
        ];
        for message in messages {
            logs.tx
                .send(CollectorEvent::Timed {
                    host: "one".into(),
                    chunk: Box::new(timed("one", message, (None, 0), false)),
                    bytes: 0,
                })
                .unwrap();
        }
        logs.tick(&rt, false);
        let text = logs.visible_text();
        assert!(text.contains("start"));
        assert!(text.contains("delayed stack"));
        assert!(text.contains("end"));
        assert_eq!(logs.rows.iter().filter(|row| row.start).count(), 2);
        assert_eq!(logs.scroll, logs.matches.len().saturating_sub(2));
        assert!(!logs.combined_dirty);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn tracked_files_remain_visible_when_stale_and_untracked_require_discovery() {
        let mut logs = viewer();
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 14)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        let mut empty = named_file("one", "8040_RequestLog.txt", "2026-10-14T01:00:00Z");
        empty.size = 0;
        logs.files = vec![
            named_file("one", "ErrorLog.txt", "2026-10-01T12:00:00Z"), // tracked, stale
            named_file("one", "9023_ErrorLog.txt", "2026-10-14T01:00:00Z"),
            named_file("one", "9023_ErrorLog_1.txt", "2026-10-12T12:00:00Z"),
            named_file("one", "8040_AccessLog.txt", "2026-10-14T01:00:00Z"),
            empty,
        ];
        assert_eq!(logs.visible_file_indices(), vec![0]);
        logs.show_untracked = true;
        assert_eq!(logs.visible_file_indices(), vec![0, 1, 3, 4]);
        assert_eq!(logs.available_file_count(), 4);
        logs.cycle_port("9023");
        assert_eq!(logs.port_filter_summary(), "+9023 (only +)");
        assert_eq!(logs.visible_file_indices(), vec![1]);
        assert_eq!(logs.available_file_count(), 4);
        logs.cycle_port("9023");
        assert_eq!(logs.visible_file_indices(), vec![0, 3, 4]);
        assert_eq!(logs.port_filter_summary(), "-9023 (all except -)");
        logs.clear_port_filters();
        assert_eq!(logs.visible_file_indices(), vec![0, 1, 3, 4]);
        logs.scope_days = 7;
        assert_eq!(logs.scope_start(), "2026-10-08T00:00:00");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn discovery_filters_files_by_rotation_suffix_not_modification_time() {
        let mut logs = viewer();
        logs.show_untracked = true;
        logs.files = vec![
            named_file("one", "ErrorLog.txt", "2026-10-01T12:00:00Z"),
            named_file("one", "8001_AccessLog_1.txt", "2026-10-14T01:00:00Z"),
            named_file("one", "8002_AccessLog_2.txt", "2026-10-13T01:00:00Z"),
            named_file("one", "8003_AccessLog_1.txt", "2026-10-14T01:00:00Z"),
        ];
        assert_eq!(logs.discovery_day_label(), "today 2026-10-14");
        // The unsuffixed file is today's file despite its old modification
        // time. Rotated files are excluded despite today's modification time.
        assert_eq!(logs.visible_file_indices(), vec![0]);
        logs.discovery_days_ago = 1;
        assert_eq!(logs.discovery_day_label(), "yesterday 2026-10-13");
        assert_eq!(logs.visible_file_indices(), vec![1, 3]);
        logs.discovery_days_ago = 2;
        assert_eq!(logs.visible_file_indices(), vec![2]);
        logs.show_untracked = false;
        assert!(logs.visible_file_indices().is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn old_inventory_results_do_not_replace_the_selected_day() {
        let mut logs = viewer();
        logs.show_untracked = true;
        logs.discovery_revision = 2;
        logs.files = vec![
            named_file("one", "ErrorLog.txt", "2026-10-01T12:00:00Z"),
            named_file("one", "8001_AccessLog.txt", "2026-10-14T01:00:00Z"),
            named_file("one", "8002_AccessLog_1.txt", "2026-10-13T01:00:00Z"),
        ];
        let runtime = Runtime::new().unwrap();
        let days_ago = 0;
        let make_response = || {
            let mut value = response("one", vec![]);
            value.files = vec![named_file(
                "one",
                "8003_AccessLog.txt",
                "2026-10-14T01:00:00Z",
            )];
            value
        };
        logs.tx
            .send(CollectorEvent::Inventory {
                host: "one".into(),
                result: Ok(make_response()),
                bytes: 0,
                revision: 1,
                days_ago,
                show_untracked: true,
            })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(logs.files[1].present);
        assert_eq!(logs.files.len(), 3);

        logs.tx
            .send(CollectorEvent::Inventory {
                host: "one".into(),
                result: Ok(make_response()),
                bytes: 0,
                revision: 2,
                days_ago,
                show_untracked: true,
            })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(
            !logs
                .files
                .iter()
                .find(|file| file.name == "8001_AccessLog.txt")
                .unwrap()
                .present
        );
        assert!(
            logs.files
                .iter()
                .find(|file| file.name == "8002_AccessLog_1.txt")
                .unwrap()
                .present
        );
        assert!(
            logs.files
                .iter()
                .find(|file| file.name == "ErrorLog.txt")
                .unwrap()
                .present
        );
        assert!(
            logs.files
                .iter()
                .any(|file| file.name == "8003_AccessLog.txt")
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn sidebar_compacts_hosts_and_temporarily_filters_files() {
        let mut logs = viewer();
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 14)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        logs.files = vec![
            named_file(
                "z-host.example",
                "9060_AccessLog.txt",
                "2026-10-14T01:00:00Z",
            ),
            named_file("a-host.example", "ErrorLog.txt", "2026-10-14T01:00:00Z"),
            named_file(
                "a-host.example",
                "9060_ErrorLog.txt",
                "2026-10-14T01:00:00Z",
            ),
        ];
        logs.show_untracked = true;
        logs.sort_files();
        assert_eq!(logs.host_alias("a-host.example"), "h1");
        assert_eq!(logs.host_alias("z-host.example"), "h2");
        assert_eq!(logs.compact_file_label(&logs.files[1]), "9060_E");
        assert_eq!(
            logs.host_legend(),
            "h1=a-host.example  ·  h2=z-host.example"
        );
        assert_eq!(
            logs.host_legend_lines(),
            ["h1=a-host.example", "h2=z-host.example"]
        );
        logs.file_filter = "9060_e".into();
        assert_eq!(logs.visible_file_indices(), vec![1]);
        logs.clear_file_filter();
        assert_eq!(logs.visible_file_indices(), vec![0, 1, 2]);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn discovery_requested_during_sync_is_queued() {
        let mut logs = viewer();
        logs.busy = true;
        let rt = Runtime::new().unwrap();

        logs.refresh(&rt);

        assert!(logs.refresh_queued);
        assert!(logs.message.contains("queued"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn scope_window_starts_at_midnight_server_time() {
        let now = NaiveDate::from_ymd_opt(2026, 3, 1)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap();
        assert_eq!(scope_start_for(now, 1), "2026-03-01T00:00:00");
        assert_eq!(scope_start_for(now, 2), "2026-02-28T00:00:00");
    }

    #[test]
    fn error_scope_discards_old_records_but_keeps_continuations() {
        let text = concat!(
            "2026-10-13 23:59:59.000 Error: old\n",
            "  old continuation\n",
            "2026-10-14 00:00:00.000 Error: current\n",
            "  current continuation\n",
        );
        assert_eq!(
            error_text_since(text, "2026-10-14T00:00:00"),
            concat!(
                "2026-10-14 00:00:00.000 Error: current\n",
                "  current continuation\n",
            )
        );
    }

    const LOG: &str = "2026-10-14 00:01:00.100 Info: a\n2026-10-14 00:02:00.419 Warning: b\n  continued\n2026-10-14 00:02:00.419 Info: c\n";

    #[test]
    fn dedupe_drops_held_records_but_keeps_new_ones_with_the_same_timestamp() {
        let all = dedupe_records(LOG, None, 0);
        assert_eq!(all.text, LOG);
        assert_eq!(all.last_ts.as_deref(), Some("2026-10-14 00:02:00.419"));
        assert_eq!(all.last_count, 2);
        // Held through the first record at 00:02:00.419 (with continuation).
        let next = dedupe_records(
            &LOG[LOG.find("2026-10-14 00:02").unwrap()..],
            Some("2026-10-14 00:02:00.419"),
            1,
        );
        assert_eq!(next.text, "2026-10-14 00:02:00.419 Info: c\n");
        assert_eq!(next.last_count, 2);
        // Nothing new: everything at the cursor is already held.
        let quiet = dedupe_records(
            &LOG[LOG.find("2026-10-14 00:02").unwrap()..],
            Some("2026-10-14 00:02:00.419"),
            2,
        );
        assert!(quiet.text.is_empty());
        // Without trailing newline (as the management API returns it).
        assert_eq!(dedupe_records(LOG.trim_end(), None, 0).text, LOG);
        assert!(record_ts("not a log line at all, but long enough").is_none());
    }

    fn timed(host: &str, text: &str, skip: (Option<&str>, usize), reset: bool) -> TimedChunk {
        let d = dedupe_records(text, skip.0, skip.1);
        TimedChunk {
            request: TimedRequest {
                host: host.into(),
                path: file(host).path,
                name: "ErrorLog.txt".into(),
                start: "2026-10-14T00:00:00".into(),
                skip_ts: skip.0.map(str::to_owned),
                skip_count: skip.1,
                reset,
            },
            text: d.text,
            last_ts: d.last_ts,
            last_count: d.last_count,
            size: None,
            modified: None,
            error: None,
        }
    }

    #[test]
    fn timed_streams_append_only_new_data_and_resume_from_the_cursor() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.apply_timed(timed("one", LOG, (None, 0), false), true)
            .unwrap();
        let key = file("one").key();
        let stem = hash_key(&key);
        let log_path = logs.cache_root.join(format!("{stem}.log"));
        assert_eq!(fs::read_to_string(&log_path).unwrap(), LOG);
        let json_len = fs::metadata(logs.cache_root.join(format!("{stem}.json")))
            .unwrap()
            .len();
        assert!(json_len < 600, "metadata must not carry the text");
        let time = logs.snapshots[&key].time.clone().unwrap();
        let request = timed_request(
            &file("one"),
            logs.snapshots.get(&key),
            "2026-10-14T00:00:00",
        );
        assert_eq!(request.start, "2026-10-14T00:02:00.419");
        assert_eq!((request.skip_count, time.count), (2, 2));

        // Next poll returns the overlap plus one new record.
        let more = format!(
            "{}2026-10-14 00:03:00.000 Error: d\n",
            &LOG[LOG.find("2026-10-14 00:02").unwrap()..]
        );
        logs.apply_timed(
            timed("one", &more, (Some("2026-10-14 00:02:00.419"), 2), false),
            true,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&log_path).unwrap(),
            format!("{LOG}2026-10-14 00:03:00.000 Error: d\n")
        );
        assert_eq!(logs.snapshots[&key].time.as_ref().unwrap().count, 1);
        // Quiet poll changes nothing.
        let before = fs::read(&log_path).unwrap();
        logs.apply_timed(
            timed(
                "one",
                "2026-10-14 00:03:00.000 Error: d",
                (Some("2026-10-14 00:03:00.000"), 1),
                false,
            ),
            true,
        )
        .unwrap();
        assert_eq!(fs::read(&log_path).unwrap(), before);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn quiet_tracked_scan_refreshes_the_last_known_sidebar_size() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        let mut quiet = timed("one", "", (None, 0), false);
        quiet.size = Some(4_096);
        quiet.modified = Some("2026-10-14T12:34:56Z".into());

        logs.apply_timed(quiet, true).unwrap();

        let file = logs.selected_file().unwrap();
        assert_eq!(file.size, 4_096);
        assert_eq!(file.modified, "2026-10-14T12:34:56Z");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn clearing_cache_resets_to_today_and_keeps_subscription_settings() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.apply_timed(timed("one", LOG, (None, 0), false), true)
            .unwrap();
        logs.scope_days = 7;
        logs.extra_streams.insert("8040_ErrorLog.txt".into());
        logs.save_subscriptions();
        fs::write(logs.cache_root.join("saved-view.txt"), "rendered view").unwrap();

        let runtime = Runtime::new().unwrap();
        logs.clear_cached_logs(&runtime);

        assert!(logs.snapshots.is_empty());
        assert!(logs.lines.is_empty());
        assert_eq!(logs.scope_days, 1);
        assert!(logs.cache_root.join(SUBSCRIPTIONS_FILE).exists());
        assert!(!logs.cache_root.join("saved-view.txt").exists());
        let saved: Value =
            serde_json::from_slice(&fs::read(logs.cache_root.join(SUBSCRIPTIONS_FILE)).unwrap())
                .unwrap();
        assert!(saved.get("scope_days").is_none());
        assert_eq!(saved["extra"], serde_json::json!(["8040_ErrorLog.txt"]),);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn view_is_extended_incrementally_and_search_applies_to_new_lines() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.search = "error".into();
        logs.apply_timed(
            timed(
                "one",
                "2026-10-14 00:01:00.000 Error: x\n2026-10-14 00:01:01.000 Info: y\n",
                (None, 0),
                false,
            ),
            true,
        )
        .unwrap();
        assert_eq!((logs.lines.len(), logs.matches.clone()), (2, vec![0]));
        logs.apply_timed(
            timed(
                "one",
                "2026-10-14 00:01:01.000 Info: y\n2026-10-14 00:01:02.000 Error: z\n",
                (Some("2026-10-14 00:01:01.000"), 1),
                false,
            ),
            true,
        )
        .unwrap();
        assert_eq!(logs.lines.len(), 3);
        assert_eq!(logs.matches, vec![0, 2]);
        assert_eq!(logs.scroll, 1, "pinned view follows new matches");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn changing_the_window_replaces_the_generation_without_fetching_a_gap() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.apply_timed(timed("one", LOG, (None, 0), false), true)
            .unwrap();
        let key = file("one").key();
        let req = |logs: &LogViewer, scope: &str| {
            timed_request(&file("one"), logs.snapshots.get(&key), scope)
        };
        assert!(
            req(&logs, "2026-10-13T00:00:00").reset,
            "an earlier window than the one held refetches"
        );
        assert!(!req(&logs, "2026-10-14T00:00:00").reset);
        // A new day is a new, bounded generation; never backfill the time away.
        let tomorrow = req(&logs, "2026-10-15T00:00:00");
        assert!(tomorrow.reset);
        assert_eq!(tomorrow.start, "2026-10-15T00:00:00");
        assert_eq!(tomorrow.skip_count, 0);
        // A widened window replaces the generation, keeping the old one by rename.
        logs.snapshots
            .get_mut(&key)
            .unwrap()
            .time
            .as_mut()
            .unwrap()
            .since = "2026-10-15T00:00:00".into();
        assert!(req(&logs, "2026-10-14T00:00:00").reset);
        let reset = timed(
            "one",
            "2026-10-14 00:10:00.000 Info: fresh\n",
            (None, 0),
            true,
        );
        logs.apply_timed(reset, true).unwrap();
        let stem = hash_key(&key);
        assert!(
            fs::read_to_string(logs.cache_root.join(format!("{stem}.0.previous.log")))
                .unwrap()
                .contains("Warning: b")
        );
        assert_eq!(
            fs::read_to_string(logs.local_path().unwrap()).unwrap(),
            "2026-10-14 00:10:00.000 Info: fresh\n"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn restart_restores_committed_text_and_ignores_uncommitted_appends() {
        let mut logs = viewer();
        logs.apply(response("one", vec![])).unwrap();
        logs.apply_timed(timed("one", LOG, (None, 0), false), true)
            .unwrap();
        let key = file("one").key();
        let log_path = logs.cache_root.join(format!("{}.log", hash_key(&key)));
        // Crash after the append but before the cursor was written.
        fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .unwrap()
            .write_all(b"2026-10-14 09:00:00.000 Info: orphan\n")
            .unwrap();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(restored.snapshots[&key].text, LOG);
        assert_eq!(restored.snapshots[&key].time.as_ref().unwrap().count, 2);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn subscriptions_persist_but_window_resets_to_today_and_legacy_snapshots_load() {
        let mut logs = viewer();
        logs.extra_streams.insert("8040_ErrorLog.txt".into());
        logs.disabled_streams
            .insert("TaskServer_ErrorLog.txt".into());
        logs.scope_days = 7;
        logs.save_subscriptions();
        let legacy = json!({"file": file("one"), "start":0, "end":4, "length":4, "anchor_start":0, "anchor":"old\n", "text":"old\n", "generation":0});
        let stem = hash_key(&file("one").key());
        fs::write(
            logs.cache_root.join(format!("{stem}.json")),
            legacy.to_string(),
        )
        .unwrap();
        fs::write(logs.cache_root.join(format!("{stem}.log")), "old\n").unwrap();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert!(restored.extra_streams.contains("8040_ErrorLog.txt"));
        assert!(
            restored
                .disabled_streams
                .contains("TaskServer_ErrorLog.txt")
        );
        assert_eq!(restored.scope_days, 1);
        assert_eq!(restored.lines.len(), 1);
        assert!(restored.display_row(0).to_string().ends_with("old"));
        // An error log cached by byte offsets migrates to a time generation.
        let req = timed_request(
            &file("one"),
            restored.snapshots.get(&file("one").key()),
            "2026-10-14T00:00:00",
        );
        assert!(req.reset);
        // Legacy appends extend the same .log without rewriting it.
        restored
            .apply(response("one", vec![chunk("one", 4, "new\n", false)]))
            .unwrap();
        assert_eq!(
            fs::read_to_string(restored.local_path().unwrap()).unwrap(),
            "old\nnew\n"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn error_log_names_use_the_management_api() {
        for name in [
            "ErrorLog.txt",
            "8000_ErrorLog.txt",
            "TaskServer_ErrorLog_3.txt",
            "ErrorLog_12.txt",
        ] {
            assert!(is_error_log(name), "{name}");
        }
        for name in [
            "8000_AccessLog.txt",
            "AuditLog.txt",
            "8000_RequestLog_2.txt",
        ] {
            assert!(!is_error_log(name), "{name}");
        }
    }

    #[test]
    fn cursors_count_unicode_characters_and_reassemble_partial_lines() {
        let first =
            merge_chunk(file("one"), None, &chunk("one", 0, "café 🦀\npart", true)).unwrap();
        let second = merge_chunk(
            file("one"),
            Some(&first),
            &chunk("one", first.end, "ial\n", false),
        )
        .unwrap();
        assert_eq!(second.text, "café 🦀\npartial\n");
        assert_ne!(second.end as usize, second.text.len());
        let unchanged = merge_chunk(
            file("one"),
            Some(&second),
            &chunk("one", second.end, "", false),
        )
        .unwrap();
        assert_eq!(unchanged.text, second.text);
    }

    #[test]
    fn rejects_gaps_overlaps_and_invalid_lengths_without_advancing_cursor() {
        let initial = merge_chunk(file("one"), None, &chunk("one", 0, "abc", true)).unwrap();
        assert!(merge_chunk(file("one"), Some(&initial), &chunk("one", 2, "def", false)).is_err());
        assert!(merge_chunk(file("one"), Some(&initial), &chunk("one", 4, "def", false)).is_err());
        let mut bad = chunk("one", 3, "🦀", false);
        bad.end += 3;
        bad.length = bad.end;
        assert!(merge_chunk(file("one"), Some(&initial), &bad).is_err());
        assert_eq!(initial.end, 3);
    }

    #[test]
    fn rotation_preserves_old_log_and_restart_restores_cursor_and_raw_content() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "old\n", true)]))
            .unwrap();
        logs.apply(response("one", vec![chunk("one", 0, "new 🦀\n", true)]))
            .unwrap();
        let stem = hash_key(&file("one").key());
        assert_eq!(
            fs::read_to_string(logs.cache_root.join(format!("{stem}.0.previous.log"))).unwrap(),
            "old\n"
        );
        assert_eq!(
            fs::read_to_string(logs.local_path().unwrap()).unwrap(),
            "new 🦀\n"
        );
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(restored.lines.len(), 1);
        assert!(restored.display_row(0).to_string().ends_with("new 🦀"));
        assert_eq!(restored.snapshots[&file("one").key()].end, 6);
        assert!(!restored.files[0].present);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn partial_inventory_keeps_other_files_and_search_can_span_hosts() {
        let mut logs = viewer();
        logs.apply(response(
            "one",
            vec![chunk("one", 0, "Warning: Alpha\n", true)],
        ))
        .unwrap();
        logs.apply(response(
            "two",
            vec![chunk("two", 0, "Error: ALPHA\n", true)],
        ))
        .unwrap();
        logs.apply(response("one", vec![])).unwrap();
        assert_eq!(logs.files.iter().filter(|f| f.present).count(), 2);
        logs.toggle_all_cached();
        logs.search = "alpha".into();
        logs.filter();
        assert_eq!(logs.matches.len(), 2);
        assert!(
            logs.display_row(0)
                .to_string()
                .contains("one / ErrorLog.txt")
        );
        assert!(
            logs.display_row(1)
                .to_string()
                .contains("two / ErrorLog.txt")
        );
        logs.search = "[".into();
        logs.filter();
        assert_eq!(logs.matches.len(), 2); // literal, no regex
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn failed_file_does_not_advance_its_cursor_or_hide_host_errors() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "abc", true)]))
            .unwrap();
        let mut failed = chunk("one", 3, "", false);
        failed.error = Some("permission denied".into());
        logs.apply(response("one", vec![failed])).unwrap();
        logs.apply(response("two", vec![])).unwrap();
        assert!(logs.errors["one"].contains("permission denied"));
        assert_eq!(logs.snapshots[&file("one").key()].end, 3);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn download_progress_identifies_files_and_keeps_failures_across_same_host_successes() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        logs.files.push(file("one"));
        logs.begin_activity("Refreshing logs", 2);
        logs.activity.as_mut().unwrap().download = true;
        logs.tx
            .send(CollectorEvent::DownloadStarted {
                host: "one".into(),
                file: "FirstLog.txt".into(),
            })
            .unwrap();
        logs.tx
            .send(CollectorEvent::DownloadStarted {
                host: "one".into(),
                file: "ErrorLog.txt".into(),
            })
            .unwrap();
        logs.tick(&runtime, true);
        let status = logs.activity_status().unwrap();
        assert!(status.contains("0 ok, 0 failed, 0 queued"), "{status}");
        assert!(status.contains("h1:FirstLog.txt"), "{status}");
        assert!(status.contains("h1:ErrorLog.txt"), "{status}");
        logs.tx
            .send(CollectorEvent::DownloadReport {
                host: "one".into(),
                file: "FirstLog.txt".into(),
                result: Err("timeout".into()),
                bytes: 0,
            })
            .unwrap();
        logs.tick(&runtime, true);
        let status = logs.activity_status().unwrap();
        assert!(status.contains("0 ok, 1 failed"), "{status}");
        assert!(status.contains("h1:ErrorLog.txt"), "{status}");
        assert!(status.contains("last h1:FirstLog.txt failed"), "{status}");
        logs.tx
            .send(CollectorEvent::DownloadReport {
                host: "one".into(),
                file: "ErrorLog.txt".into(),
                result: Ok(response("one", vec![chunk("one", 0, "abc", true)])),
                bytes: 3,
            })
            .unwrap();
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&runtime, true);
        assert_eq!(
            logs.message,
            "Refreshing logs finished: 1 successful, 1 failed"
        );
        assert!(logs.errors["one"].contains("FirstLog.txt: timeout"));
        assert_eq!(logs.snapshots[&file("one").key()].text, "abc");
        assert_eq!(logs.host_legend_lines(), ["h1 [ok]=one"]);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn on_demand_checks_each_file_independently_through_the_same_endpoint() {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move {
            let mut checked = Vec::new();
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") { headers.push(stream.read_u8().await.unwrap()); }
                let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| {
                    line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)
                }).unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                let host = vars["expected"].as_str().unwrap();
                let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
                assert_eq!(requests.len(), 1);
                let path = requests[0]["path"].as_str().unwrap();
                let name = path.rsplit('/').next().unwrap();
                checked.push((host.to_owned(), name.to_owned()));
                let (status, response) = if name == "TaskServer_ErrorLog.txt" {
                    ("500 Internal Server Error", "read failed".to_string())
                } else {
                    ("200 OK", json!({"host":host, "directory":"/Logs", "inventory":false,
                        "read_bytes":3, "cluster_bytes":0,
                        "files":[{"host":host,"path":path,"name":name,"size":3,"modified":"2026-10-14T09:00:00Z"}],
                        "chunks":[{"host":host,"path":path,"start":0,"end":3,"length":3,"reset":true,"text":"abc","full":true}]
                    }).to_string())
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
            checked
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        logs.files = vec![
            file("one"),
            file("two"),
            named_file("one", "TaskServer_ErrorLog.txt", "2026-10-14T09:00:00Z"),
        ];
        logs.endpoints = ["one", "two"]
            .into_iter()
            .map(|host| Endpoint {
                host: host.into(),
                client: client.clone(),
            })
            .collect();
        logs.errors.insert("one".into(), "old error".into());
        logs.sync_on_demand(&runtime, None);
        assert!(logs.errors.is_empty());
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while logs.busy {
                    logs.tick(&runtime, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                assert_eq!(server.await.unwrap().len(), 3);
            })
            .await
            .unwrap();
        });
        assert_eq!(
            logs.message,
            "Refreshing logs finished: 2 successful, 1 failed"
        );
        assert!(logs.errors["one"].contains("TaskServer_ErrorLog.txt"));
        assert!(logs.errors["one"].contains("read failed"));
        assert!(!logs.errors["one"].contains("old error"));
        assert_eq!(logs.snapshots[&file("one").key()].text, "abc");
        assert_eq!(logs.snapshots[&file("two").key()].text, "abc");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn download_progress_stays_visible_until_done_and_resets_on_cancel() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        let received = logs.begin_activity("Downloading logs", 2);
        received.store(4096, Ordering::Relaxed);
        logs.activity.as_mut().unwrap().started = Instant::now() - Duration::from_secs(3);
        logs.tx
            .send(CollectorEvent::Report {
                host: "one".into(),
                result: Err("permission denied".into()),
                bytes: 4096,
            })
            .unwrap();
        logs.tick(&runtime, true);
        let status = logs.activity_status().unwrap();
        assert!(
            status.contains("Downloading logs · 1/2 complete · 4.0 KiB received · 3s"),
            "{status}"
        );
        assert!(status.contains("1 host error(s)"));

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let footer: String = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 23)].symbol())
            .collect();
        assert!(footer.contains("Downloading logs · 1/2 complete"));
        assert!(footer.contains("4.0 KiB received"));

        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(logs.activity_status().is_none());
        assert!(logs.message.contains("permission denied"));
        logs.begin_activity("Refreshing logs", 1);
        logs.cancel();
        assert!(logs.activity_status().is_none());
        logs.begin_activity("Refreshing logs", 1);
        received.fetch_add(4096, Ordering::Relaxed);
        assert!(logs.activity_status().unwrap().contains("0 B received"));
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn cache_failure_remains_visible_after_download_completes() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        let mut invalid = chunk("one", 0, "abc", true);
        invalid.end = 20;
        logs.begin_activity("Downloading logs", 1);
        logs.tx
            .send(CollectorEvent::Report {
                host: "one".into(),
                result: Ok(response("one", vec![invalid])),
                bytes: 0,
            })
            .unwrap();
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(logs.errors["one"].contains("Cache error: Invalid log chunk"));
        assert!(!logs.busy);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn cancellation_keeps_completed_chunks_and_accounts_received_bodies() {
        let mut logs = viewer();
        logs.counter.store(123, Ordering::Relaxed);
        logs.tx
            .send(CollectorEvent::Report {
                host: "one".into(),
                result: Ok(response("one", vec![chunk("one", 0, "abc", true)])),
                bytes: 123,
            })
            .unwrap();
        logs.cancel();
        assert_eq!(logs.body_bytes, 123);
        assert_eq!(logs.host_bytes["one"], 123);
        assert_eq!(logs.lines.len(), 1);
        assert!(logs.display_row(0).to_string().ends_with("abc"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[tokio::test]
    async fn digest_challenge_and_multipart_body_are_counted_and_host_mismatches_fail() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let json=serde_json::to_string(&json!({"host":"wrong", "directory":"/Logs", "inventory":true,"read_bytes":0,"cluster_bytes":0,"files":[],"chunks":[]})).unwrap();
        let body = format!(
            "--boundary\r\nContent-Type: application/json\r\n\r\n{json}\r\n--boundary--\r\n"
        );
        let expected_bytes = body.len() as u64 + 9;
        let server = tokio::spawn(async move {
            for (status, extra, body) in [
                (
                    "401 Unauthorized",
                    "WWW-Authenticate: Digest realm=\"logs\", nonce=\"test\", qop=\"auth\"\r\n",
                    "challenge".to_string(),
                ),
                (
                    "200 OK",
                    "Content-Type: multipart/mixed; boundary=boundary\r\n",
                    body,
                ),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0; 4096];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    request.extend_from_slice(&buffer[..count]);
                    if let Some(start) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..start]);
                        let length: usize = headers
                            .lines()
                            .find_map(|l| {
                                l.to_lowercase()
                                    .strip_prefix("content-length: ")
                                    .map(str::to_owned)
                            })
                            .unwrap()
                            .parse()
                            .unwrap();
                        if request.len() >= start + 4 + length {
                            break;
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request);
                assert!(request.starts_with("POST /v1/eval HTTP/1.1"));
                assert!(request.contains("vars="));
                assert!(!request.contains("database="));
                if status.starts_with("200") {
                    assert!(request.to_lowercase().contains("authorization: digest"));
                }
                stream.write_all(format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let counter = Arc::new(AtomicU64::new(0));
        let client = MarkLogicClient::new(ServerConfig {
            name: "test".into(),
            host: "127.0.0.1".into(),
            username: "test".into(),
            password: "test".into(),
            port,
            secure: false,
            auth_type: AuthType::Digest,
            insecure: false,
            app_servers: vec![],
            uri: None,
        })
        .unwrap()
        .with_download_counter(counter.clone());
        let result = collect(
            &Endpoint {
                host: "one".into(),
                client,
            },
            "",
            &[],
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("Host mismatch"));
        assert_eq!(counter.load(Ordering::Relaxed), expected_bytes);
        server.await.unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires MARKLOGIC_LOG_TEST_CONFIG; read-only, downloads small ErrorLog files"]
    async fn live_on_demand_small_logs_through_one_endpoint() {
        let path = std::env::var("MARKLOGIC_LOG_TEST_CONFIG").unwrap();
        let config: crate::config::AppConfig =
            toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let server = config.effective_active_server_config().unwrap();
        let client = MarkLogicClient::new(server).unwrap();
        let discovery: Discovery =
            serde_json::from_value(client.logs_eval(DISCOVER, &json!({})).await.unwrap()).unwrap();
        let names = BTreeSet::from(["ErrorLog.txt".to_string()]);
        let mut logs = viewer();
        for host in discovery.hosts {
            let endpoint = Endpoint {
                host: host.clone(),
                client: client.clone(),
            };
            let found = inventory(&endpoint, "", 0, false, &names).await.unwrap();
            let Some(file) = found
                .files
                .iter()
                .find(|file| file.name == "ErrorLog.txt" && file.size > 0 && file.size < 1_000_000)
                .cloned()
            else {
                continue;
            };
            logs.apply(found).unwrap();
            let requests = logs.on_demand_requests(Some("ErrorLog.txt"));
            let response = collect_since(&endpoint, "", &requests[&host], "")
                .await
                .unwrap();
            assert!(
                response.chunks.iter().all(|chunk| chunk.error.is_none()),
                "{host}: {:?}",
                response.chunks
            );
            logs.apply(response).unwrap();
            assert!(
                logs.snapshots
                    .get(&file.key())
                    .is_some_and(|snapshot| !snapshot.text.is_empty()),
                "{host}"
            );
            println!("{host}: downloaded and cached {} bytes", file.size);
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[tokio::test]
    #[ignore = "Requires an explicitly supplied local MarkLogic config; only reads log files"]
    async fn live_cluster_log_extraction() {
        let path = std::env::var("MARKLOGIC_LOG_TEST_CONFIG")
            .expect("Set MARKLOGIC_LOG_TEST_CONFIG to a local TUI config.toml");
        let config: crate::config::AppConfig =
            toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let server = config.effective_active_server_config().unwrap();
        assert!(
            matches!(server.host.as_str(), "localhost" | "127.0.0.1"),
            "Live test is restricted to localhost"
        );
        let counter = Arc::new(AtomicU64::new(0));
        let client = MarkLogicClient::new(server)
            .unwrap()
            .with_download_counter(counter.clone());
        let discovery: Discovery =
            serde_json::from_value(client.logs_eval(DISCOVER, &json!({})).await.unwrap()).unwrap();
        let names = BTreeSet::from(["ErrorLog.txt".to_string()]);
        for host in discovery.hosts {
            let endpoint = Endpoint {
                host: host.clone(),
                client: client.clone(),
            };
            let tracked_only = inventory(&endpoint, "", 0, false, &names).await.unwrap();
            assert!(
                tracked_only
                    .files
                    .iter()
                    .all(|file| names.contains(&file.name))
            );
            let today = inventory(&endpoint, "", 0, true, &names).await.unwrap();
            assert!(today.files.iter().all(|file| {
                names.contains(&file.name) || LogViewer::file_days_ago(file) == Some(0)
            }));
            let inventory = collect(&endpoint, "", &[]).await.unwrap();
            assert!(inventory.inventory);
            let Some(file) = inventory.files.iter().find(|f| f.name == "ErrorLog.txt") else {
                continue;
            };
            let tail = collect(
                &endpoint,
                "",
                &[json!({"path":file.path,"offset":-1,"anchor_start":0,"anchor":"","full":false})],
            )
            .await
            .unwrap();
            let chunk = &tail.chunks[0];
            assert!(chunk.error.is_none(), "{:?}", chunk.error);
            let snapshot = merge_chunk(file.clone(), None, chunk).unwrap();
            assert!(chunk.text.chars().count() <= 65536);
            let repeat = collect(&endpoint, "", &[snapshot.request(false)])
                .await
                .unwrap();
            assert!(
                repeat.chunks.iter().all(|c| !c.reset),
                "Unchanged file reset its cursor"
            );
            assert!(repeat.chunks.iter().all(|c| c.start == snapshot.end));
            let requests: Vec<_> = inventory.files.iter().filter(|f| f.name.ends_with("Log.txt"))
                .map(|f| json!({"path":f.path,"offset":-1,"anchor_start":0,"anchor":"","full":false})).collect();
            let batch = collect(&endpoint, "", &requests).await.unwrap();
            assert_eq!(batch.chunks.len(), requests.len());
            assert!(batch.chunks.iter().all(|c| c.error.is_none()));
            assert!(
                batch
                    .chunks
                    .iter()
                    .map(|c| c.text.chars().count())
                    .sum::<usize>()
                    <= 524288
            );
            let full = collect(&endpoint, "", &[snapshot.request(true)])
                .await
                .unwrap();
            let full = &full.chunks[0];
            assert_eq!(full.start, 0);
            assert_eq!(full.end, full.length);
            assert_eq!(full.text.chars().count() as u64, full.length);
            println!(
                "{host}: {} files; tail {} UTF-8 bytes; repeat {} log bytes",
                inventory.files.len(),
                chunk.text.len(),
                repeat.chunks.iter().map(|c| c.text.len()).sum::<usize>()
            );
        }
        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    #[ignore = "Requires MARKLOGIC_LOG_TEST_CONFIG pointing at a localhost config.toml; read-only"]
    async fn live_time_window_tracking_is_incremental() {
        let path = std::env::var("MARKLOGIC_LOG_TEST_CONFIG").expect("config.toml path");
        let config: crate::config::AppConfig =
            toml::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let server = config.effective_active_server_config().unwrap();
        assert!(matches!(server.host.as_str(), "localhost" | "127.0.0.1"));
        let counter = Arc::new(AtomicU64::new(0));
        let client = MarkLogicClient::new(server)
            .unwrap()
            .with_download_counter(counter.clone());
        let discovery: Discovery =
            serde_json::from_value(client.logs_eval(DISCOVER, &json!({})).await.unwrap()).unwrap();
        let mut logs = viewer();
        logs.clock = Some((
            DateTime::parse_from_rfc3339(&discovery.now)
                .unwrap()
                .naive_local(),
            Instant::now(),
        ));
        logs.scope_days = 2;
        for host in &discovery.hosts {
            let endpoint = Endpoint {
                host: host.clone(),
                client: client.clone(),
            };
            logs.apply(collect(&endpoint, "", &[]).await.unwrap())
                .unwrap();
            logs.endpoints.push(endpoint);
        }
        for round in 0..2 {
            let before = counter.load(Ordering::Relaxed);
            for plan in logs.plan(None) {
                for request in plan.timed {
                    let chunk = fetch_timed(&client, "", request).await;
                    assert!(chunk.error.is_none(), "{:?}", chunk.error);
                    logs.apply_timed(chunk, true).unwrap();
                }
            }
            let bytes = counter.load(Ordering::Relaxed) - before;
            let streams = logs.snapshots.values().filter(|s| s.time.is_some()).count();
            println!(
                "round {round}: {bytes} body bytes, {streams} streams, {} cached bytes",
                logs.cached_total()
            );
            if round == 0 {
                assert!(streams > 0 && logs.cached_total() > 0);
            }
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
}
