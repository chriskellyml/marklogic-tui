//! Host-scoped query collectors and a persistent, locally searchable log cache.
use crate::client::MarkLogicClient;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as Days, NaiveDate, NaiveDateTime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::Write,
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
use ratatui::text::Line;

pub(crate) struct ViewRow {
    pub line: Line<'static>,
    pub timestamp: Option<NaiveDateTime>,
    pub error: bool,
    pub start: bool,
    pub search_match: bool,
}

const DISCOVER: &str = include_str!("logs/discover.xqy");
const COLLECT: &str = include_str!("logs/collect.xqy");
const SCAN: &str = include_str!("logs/scan.xqy");
/// Records per scan round, and rounds per poll, bounding each response.
const SCAN_LIMIT: usize = 20_000;
const SCAN_ROUNDS: usize = 50;
const INTERVAL: Duration = Duration::from_secs(5);
/// Inventory is re-read every N polls so rotated or new files are noticed.
const INVENTORY_EVERY: u32 = 12;
/// Streams collected without any opt-in. Everything else is opt-in.
const DEFAULT_STREAMS: [&str; 3] = [
    "8000_ErrorLog.txt",
    "ErrorLog.txt",
    "TaskServer_ErrorLog.txt",
];
const SUBSCRIPTIONS_FILE: &str = "subscriptions.cfg";
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
    Report {
        host: String,
        result: Result<HostResponse, String>,
        bytes: u64,
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

pub(crate) struct LogViewer {
    pub files: Vec<LogFile>,
    pub selected: usize,
    pub files_focus: bool,
    /// A temporary, local filter for the file sidebar. This is deliberately
    /// separate from content search so narrowing a long inventory never
    /// changes the records shown in the content pane.
    pub file_filter: String,
    pub file_filter_editing: bool,
    pub search: String,
    pub search_editing: bool,
    pub scroll: usize,
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
    pub view_width: usize,
    pub rows: Vec<ViewRow>,
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
    clock: Option<(NaiveDateTime, Instant)>,
    polls: u32,
    port_filters: BTreeMap<String, PortFilter>,
    pub include_only_marked_ports: bool,
    counter: Arc<AtomicU64>,
    pub host_bytes: BTreeMap<String, u64>,
    pub errors: BTreeMap<String, String>,
    pub cache_root: PathBuf,
    pub busy: bool,
    /// A discovery requested while a catch-up is running. It starts as soon
    /// as the current collector emits Done, so F/r never silently do nothing.
    refresh_queued: bool,
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
            files_focus: true,
            file_filter: String::new(),
            file_filter_editing: false,
            search: String::new(),
            search_editing: false,
            scroll: 0,
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
            view_width: 0,
            rows: Vec::new(),
            parsed_streams: HashMap::new(),
            combined_dirty: false,
            server_offset: FixedOffset::east_opt(0).unwrap(),
            scope_days: 1,
            view_height: 0,
            extra_streams: BTreeSet::new(),
            disabled_streams: BTreeSet::new(),
            clock: None,
            polls: 0,
            port_filters: BTreeMap::new(),
            include_only_marked_ports: false,
            counter: Arc::new(AtomicU64::new(0)),
            host_bytes: BTreeMap::new(),
            errors: BTreeMap::new(),
            cache_root: PathBuf::new(),
            busy: false,
            refresh_queued: false,
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
                ..Self::default()
            };
            self.identity = identity.clone();
            self.cache_root = dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("marklogic-tui/logs")
                .join(hash_key(&identity));
            self.directory = directory.into();
            if let Err(e) = self.load_cache() {
                self.message = format!("Cache: {e:#}");
            }
        } else if reset_to_today {
            if let Err(error) = self.prune_cached_history() {
                self.message = format!("Cache cleanup: {error:#}");
            }
            self.ensure_visible_selection();
            self.rebuild_lines();
            self.scroll_to_bottom();
        }
        self.client = Some(client.clone());
        // Opening the log view must not turn into a cluster-wide filesystem
        // inventory scan. Cached files already tell us which hosts and paths
        // the user has tracked, so they are enough to resume those streams.
        // `r` remains the explicit opt-in for discovering anything new.
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
        if self.endpoints.is_empty() {
            self.message = "Cached logs loaded. Press r to discover logs on the cluster.".into();
        } else {
            self.sync(rt, None);
        }
    }

    pub fn close(&mut self) -> crate::AppMode {
        self.cancel();
        self.opened_from.take().unwrap_or(crate::AppMode::Normal)
    }

    pub fn cancel(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.refresh_queued = false;
        while let Ok(event) = self.rx.try_recv() {
            if let CollectorEvent::Report {
                host,
                result,
                bytes,
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
        // A fresh channel isolates late results from the cancelled collector.
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = tx;
        self.rx = rx;
    }

    fn load_cache(&mut self) -> Result<()> {
        fs::create_dir_all(&self.cache_root)?;
        self.load_subscriptions();
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
            let mut file = snapshot.file.clone();
            file.present = false;
            self.files.push(file);
            self.snapshots.insert(snapshot.file.key(), snapshot);
        }
        self.sort_files();
        self.prune_cached_history()?;
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
        }
        if let Ok(bytes) = fs::read(self.cache_root.join(SUBSCRIPTIONS_FILE))
            && let Ok(saved) = serde_json::from_slice::<Saved>(&bytes)
        {
            self.extra_streams = saved.extra;
            self.disabled_streams = saved.disabled;
        }
    }

    fn save_subscriptions(&mut self) {
        let value = json!({"extra":self.extra_streams, "disabled":self.disabled_streams});
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
        !self.disabled_streams.contains(&file.name)
            && (DEFAULT_STREAMS.contains(&file.name.as_str())
                || self.extra_streams.contains(&file.name))
    }

    pub fn toggle_subscription(&mut self, rt: &Runtime) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let name = file.name.clone();
        let default = DEFAULT_STREAMS.contains(&name.as_str());
        let now_on = if self.is_subscribed(&file) {
            if default {
                self.disabled_streams.insert(name.clone());
            }
            self.extra_streams.remove(&name);
            false
        } else {
            self.disabled_streams.remove(&name);
            if !default {
                self.extra_streams.insert(name.clone());
            }
            true
        };
        self.save_subscriptions();
        // Tracking is a priority decision as well as a collection decision:
        // keep the selected file stable while moving its stream into (or out
        // of) the leading tracked group immediately.
        self.sort_files();
        if self.combined {
            self.rebuild_lines();
        }
        self.message = if now_on {
            let note = if is_error_log(&name) {
                ""
            } else {
                " (not an error log: changed files are read in full on the server)"
            };
            format!("Tracking {name} on every host{note}.")
        } else {
            format!("No longer tracking {name}; cached content is kept.")
        };
        if now_on {
            self.sync(rt, None);
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
        self.files.sort_by(|left, right| {
            let left_tracked = !disabled_streams.contains(&left.name)
                && (DEFAULT_STREAMS.contains(&left.name.as_str())
                    || extra_streams.contains(&left.name));
            let right_tracked = !disabled_streams.contains(&right.name)
                && (DEFAULT_STREAMS.contains(&right.name.as_str())
                    || extra_streams.contains(&right.name));
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
    pub fn selected_status(&self) -> String {
        let Some(file) = self.selected_file() else {
            return "No logs discovered yet".into();
        };
        match self.snapshots.get(&file.key()) {
            Some(s) if s.time.is_some() => {
                let time = s.time.as_ref().expect("checked");
                format!(
                    "Cached {} | {} → {} | {} | {}",
                    format_bytes(s.text.len() as u64),
                    time.since.replace('T', " "),
                    time.ts,
                    if self.is_subscribed(file) {
                        "tracked"
                    } else {
                        "on demand (o tracks)"
                    },
                    if file.present {
                        "online"
                    } else {
                        "cached / not discovered"
                    }
                )
            }
            Some(s) => format!(
                "Cached {} | characters {}..{} of {}{} | {}",
                format_bytes(s.text.len() as u64),
                s.start,
                s.end,
                s.length,
                if s.start > 0 {
                    " (tail; older data omitted)"
                } else {
                    ""
                },
                if file.present {
                    "online"
                } else {
                    "cached / not discovered"
                }
            ),
            None => format!(
                "Remote {} | Enter: fetch, o: track, b: download full file",
                format_bytes(file.size)
            ),
        }
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        let file = self.selected_file()?;
        self.snapshots.get(&file.key())?;
        Some(
            self.cache_root
                .join(format!("{}.log", hash_key(&file.key()))),
        )
    }

    pub fn select(&mut self, index: usize) {
        if !self.combined {
            self.all_cached = false;
        }
        self.selected = index.min(self.files.len().saturating_sub(1));
        self.horizontal = 0;
        self.rebuild_lines();
        self.scroll_to_bottom();
    }

    fn file_day(&self, file: &LogFile) -> Option<NaiveDate> {
        DateTime::parse_from_rfc3339(&file.modified)
            .ok()
            .map(|time| time.with_timezone(&self.server_offset).date_naive())
            .or_else(|| {
                file.modified
                    .get(..10)
                    .and_then(|day| NaiveDate::parse_from_str(day, "%Y-%m-%d").ok())
            })
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

    fn is_rotated_file(file: &LogFile) -> bool {
        file.name
            .strip_suffix(".txt")
            .and_then(|name| name.rsplit_once('_'))
            .is_some_and(|(_, suffix)| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
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
        let hosts = self
            .files
            .iter()
            .filter(|file| self.is_file_in_history_window(file))
            .map(|file| file.host.as_str())
            .collect::<BTreeSet<_>>();
        let index = hosts
            .iter()
            .position(|candidate| *candidate == host)
            .unwrap_or(0);
        format!("h{}", index + 1)
    }

    pub fn host_legend(&self) -> String {
        self.host_legend_lines().join("  ·  ")
    }

    pub fn host_legend_lines(&self) -> Vec<String> {
        self.files
            .iter()
            .filter(|file| self.is_file_in_history_window(file))
            .map(|file| file.host.as_str())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .enumerate()
            .map(|(index, host)| format!("h{}={host}", index + 1))
            .collect()
    }

    fn matches_file_filter(&self, file: &LogFile) -> bool {
        let needle = self.file_filter.trim().to_lowercase();
        needle.is_empty()
            || file.host.to_lowercase().contains(&needle)
            || file.name.to_lowercase().contains(&needle)
            || self
                .compact_file_label(file)
                .to_lowercase()
                .contains(&needle)
            || self.host_alias(&file.host).contains(&needle)
    }

    fn is_file_visible(&self, file: &LogFile) -> bool {
        if !self.is_file_in_history_window(file) {
            return false;
        }
        // Rotations are collector inputs rather than normal sidebar choices:
        // an ErrorLog scan includes them automatically to bridge midnight.
        // Keep the default list focused on current streams, but let an
        // explicit file filter surface an archive for one-off inspection.
        if Self::is_rotated_file(file) && self.file_filter.trim().is_empty() {
            return false;
        }
        // The sidebar is an inventory for the chosen window. Tracking a
        // stream means it is collected, not that every historical rotation
        // should appear in a default "today" list.
        let port_filter = Self::file_port(file).and_then(|port| self.port_filters.get(port));
        if matches!(port_filter, Some(PortFilter::Exclude)) {
            return false;
        }
        if !self.matches_file_filter(file) {
            return false;
        }
        if self.is_subscribed(file) {
            return true;
        }
        if self.include_only_marked_ports && !matches!(port_filter, Some(PortFilter::Include)) {
            return false;
        }
        true
    }

    fn is_file_in_history_window(&self, file: &LogFile) -> bool {
        if file.size == 0 {
            return false;
        }
        let floor = self.scope_start();
        self.file_day(file)
            .is_none_or(|day| day.to_string().as_str() >= &floor[..10])
    }

    pub fn visible_file_indices(&self) -> Vec<usize> {
        self.files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| self.is_file_visible(file).then_some(index))
            .collect()
    }

    pub fn window_file_count(&self) -> usize {
        self.files
            .iter()
            .filter(|file| self.is_file_in_history_window(file))
            .count()
    }

    pub fn selected_visible_index(&self) -> Option<usize> {
        self.visible_file_indices()
            .iter()
            .position(|index| *index == self.selected)
    }

    fn ensure_visible_selection(&mut self) {
        if self.selected_visible_index().is_none()
            && let Some(index) = self.visible_file_indices().first().copied()
        {
            self.select(index);
        }
    }

    pub fn select_visible(&mut self, index: usize) {
        if let Some(raw_index) = self.visible_file_indices().get(index).copied() {
            self.select(raw_index);
        }
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
        let last = self.visible_file_indices().len().saturating_sub(1);
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

    pub fn cycle_selected_port_filter(&mut self) {
        let Some(port) = self
            .selected_file()
            .and_then(Self::file_port)
            .map(str::to_string)
        else {
            self.message = "The selected log name has no port prefix to filter.".into();
            return;
        };
        self.cycle_port(&port);
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
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = match next {
            Some(PortFilter::Include) => format!("Port {port}: marked for inclusion."),
            Some(PortFilter::Exclude) => format!("Port {port}: excluded."),
            None => format!("Port {port}: no port filter."),
        };
    }

    pub fn toggle_include_only_marked_ports(&mut self) {
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
    pub fn cycle_scope_days(&mut self, rt: &Runtime) {
        self.scope_days = match self.scope_days {
            1 => 2,
            2 => 7,
            7 => 30,
            _ => 1,
        };
        self.save_subscriptions();
        let cleanup_error = self.prune_cached_history().err();
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.message = if let Some(error) = cleanup_error {
            format!(
                "History window: {}; cache cleanup: {error:#}",
                self.scope_label()
            )
        } else {
            format!(
                "History window: {} (streams holding less history are refetched; wider windows download more).",
                self.scope_label()
            )
        };
        // Inventory is also window-bounded, so widening needs a fresh
        // discovery before the newly eligible rotations can be collected.
        self.refresh(rt);
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
        self.rows.clear();
        self.lines = if self.all_cached {
            self.files
                .iter()
                .filter(|file| self.is_file_visible(file))
                .filter_map(|file| self.snapshots.get(&file.key()))
                .flat_map(|s| {
                    self.visible_snapshot_lines(s)
                        .into_iter()
                        .map(|line| format!("[{} / {}] {line}", s.file.host, s.file.name))
                        .collect::<Vec<_>>()
                })
                .collect()
        } else {
            self.selected_file()
                .and_then(|f| self.snapshots.get(&f.key()))
                .map(|s| self.visible_snapshot_lines(s))
                .unwrap_or_default()
        };
        self.filter();
    }
    pub fn toggle_all_cached(&mut self) {
        self.all_cached = !self.all_cached;
        self.rebuild_lines();
        self.scroll_to_bottom();
    }
    pub fn filter(&mut self) {
        if self.combined {
            self.rebuild_combined();
            return;
        }
        let needle = self.search.to_lowercase();
        self.matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| needle.is_empty() || line.to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect();
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
    }

    /// Put the newest visible line at the bottom of the content pane.  Before
    /// the first render the pane height is unknown; using one row still gives
    /// the renderer a tail position that it can clamp to its actual height.
    pub fn scroll_to_bottom(&mut self) {
        self.scroll = self.matches.len().saturating_sub(self.view_height.max(1));
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
                if path.file_name().and_then(|name| name.to_str()) == Some(SUBSCRIPTIONS_FILE) {
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

    fn visible_snapshot_lines(&self, snapshot: &Snapshot) -> Vec<String> {
        if is_error_log(&snapshot.file.name) {
            error_text_since(&snapshot.text, &self.scope_start())
                .lines()
                .map(str::to_owned)
                .collect()
        } else {
            snapshot.text.lines().map(str::to_owned).collect()
        }
    }

    /// Enforce the selected time window locally too. This keeps an older
    /// cache from reappearing after restart and reclaims space when the user
    /// narrows the window again.
    fn prune_cached_history(&mut self) -> Result<()> {
        let scope = self.scope_start();
        let mut changed = Vec::new();
        for (key, snapshot) in &mut self.snapshots {
            if !is_error_log(&snapshot.file.name) {
                continue;
            }
            let text = error_text_since(&snapshot.text, &scope);
            let window_changed = snapshot
                .time
                .as_ref()
                .is_some_and(|time| time.since != scope);
            if text == snapshot.text && !window_changed {
                continue;
            }
            snapshot.text = text;
            snapshot.text_bytes = snapshot.text.len() as u64;
            if let Some(time) = &mut snapshot.time {
                time.since = scope.clone();
            }
            changed.push((key.clone(), snapshot.meta(), snapshot.text.clone()));
        }
        for (key, meta, text) in changed {
            let stem = hash_key(&key);
            atomic_write(
                &self.cache_root.join(format!("{stem}.log")),
                text.as_bytes(),
            )?;
            atomic_write(
                &self.cache_root.join(format!("{stem}.json")),
                &serde_json::to_vec(&meta)?,
            )?;
        }
        self.parsed_streams.clear();
        Ok(())
    }

    pub fn toggle_combined(&mut self) {
        self.combined = !self.combined;
        self.files_focus = false;
        self.rebuild_lines();
        self.scroll_to_bottom();
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
                        Some(PortFilter::Include) => "+ include",
                        Some(PortFilter::Exclude) => "- exclude",
                        None => "  default",
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
            format!(
                "Details: {}",
                if self.options.chrome {
                    "shown"
                } else {
                    "hidden"
                }
            ),
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
            4 => self.options.chrome = !self.options.chrome,
            5 => {
                self.options.search_mode = match self.options.search_mode {
                    SearchMode::Off => SearchMode::Highlight,
                    SearchMode::Highlight => SearchMode::Filter,
                    SearchMode::Filter => SearchMode::Off,
                }
            }
            i @ 6..=9 => self.options.kinds[i - 6] = !self.options.kinds[i - 6],
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
        for (key, snapshot) in &self.snapshots {
            self.parsed_streams.entry(key.clone()).or_default().update(
                &snapshot.file,
                &snapshot.text,
                false,
                self.server_offset,
            );
        }
        let floor = presentation::parse_time(&self.scope_start(), self.server_offset);
        let mut records: Vec<_> = self
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
            .filter_map(|file| self.parsed_streams.get(&file.key()))
            .flat_map(|stream| &stream.records)
            .filter(|record| {
                self.options.kinds[match record.kind {
                    Kind::Error => 0,
                    Kind::Request => 1,
                    Kind::Access => 2,
                    Kind::Other => 3,
                }]
            })
            .filter(|record| {
                record
                    .timestamp
                    .is_none_or(|t| floor.is_none_or(|floor| t >= floor))
            })
            .collect();
        // Stable, deterministic tie order retains per-file continuations and
        // identical timestamps without removing genuine duplicate records.
        records.sort_by_key(|record| record.timestamp);
        let first = records.iter().find_map(|record| record.timestamp);
        let hosts = records
            .iter()
            .map(|r| &r.host)
            .collect::<BTreeSet<_>>()
            .len()
            > 1;
        let patterns = Patterns::new(&self.search);
        self.rows.clear();
        self.lines.clear();
        self.matches.clear();
        let mut previous_day = None;
        for record in records {
            let rendered = record.render(&self.options, first, hosts);
            let search_text = format!(
                "{} {} {} {}",
                record.host,
                record.file,
                record.raw,
                rendered
                    .iter()
                    .map(Line::to_string)
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            let search_match = patterns.matches(&search_text);
            let matched = self.options.search_mode != SearchMode::Filter || search_match;
            if matched
                && let Some(day) = record.timestamp.map(|t| t.date())
                && previous_day != Some(day)
                && !self.options.relative
            {
                previous_day = Some(day);
                let line = Line::from(ratatui::text::Span::styled(
                    format!("── {day} (server time) ──"),
                    ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
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
            }
            let mut start = true;
            for line in rendered {
                let line = if self.options.search_mode == SearchMode::Off {
                    line
                } else {
                    patterns.highlight(line)
                };
                let wrapped = if self.options.wrap {
                    presentation::wrap_line(line, self.view_width.max(1))
                } else {
                    vec![line]
                };
                for line in wrapped {
                    if matched {
                        self.matches.push(self.lines.len());
                    }
                    self.lines.push(line.to_string());
                    self.rows.push(ViewRow {
                        line,
                        timestamp: record.timestamp,
                        error: record.is_error(),
                        start,
                        search_match,
                    });
                    start = false;
                }
            }
        }
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
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
                self.seek_nearest(target);
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
            self.scroll = position;
        }
    }
    pub fn jump_time(&mut self, seconds: i64) {
        if let Some(time) = self
            .matches
            .get(self.scroll)
            .and_then(|&row| self.rows.get(row))
            .and_then(|r| r.timestamp)
        {
            self.seek_nearest(time + Days::seconds(seconds));
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
        let next = if backwards {
            eligible
                .iter()
                .rev()
                .copied()
                .find(|&i| i < self.scroll)
                .or_else(|| eligible.last().copied())
        } else {
            eligible
                .iter()
                .copied()
                .find(|&i| i > self.scroll)
                .or_else(|| eligible.first().copied())
        };
        if let Some(position) = next {
            self.scroll = position;
        }
    }
    pub fn visible_text(&self) -> String {
        self.matches
            .iter()
            .map(|&i| self.lines[i].as_str())
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
    pub fn remote_total(&self) -> u64 {
        self.files
            .iter()
            .filter(|f| f.present && self.is_file_visible(f))
            .map(|f| f.size)
            .sum()
    }
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
        self.busy = true;
        self.last_bytes = 0;
        self.message = "Discovering hosts and log sizes…".into();
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let scope_days = self.scope_days;
        let handle = rt.spawn(async move {
            let mut accounted = counter.load(Ordering::Relaxed);
            let client = client.with_download_counter(counter.clone());
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
            let scope = DateTime::parse_from_rfc3339(&discovery.now)
                .map(|now| scope_start_for(now.naive_local(), scope_days))
                .unwrap_or_else(|_| {
                    scope_start_for(chrono::Local::now().naive_local(), scope_days)
                });
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
                let result = collect_since(endpoint, &directory, &[], &scope)
                    .await
                    .map_err(|e| format!("{e:#}"));
                let bytes = take_delta(&counter, &mut accounted)
                    + if index == 0 { discovery_bytes } else { 0 };
                let _ = tx.send(CollectorEvent::Report {
                    host: endpoint.host.clone(),
                    result,
                    bytes,
                });
            }
            let _ = tx.send(CollectorEvent::Done { auto_tail: true });
        });
        self.abort = Some(handle.abort_handle());
    }

    /// Download one file from its beginning in a single eval query.
    pub fn download_full(&mut self, rt: &Runtime) {
        if self.busy {
            return;
        }
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let Some(endpoint) = self.endpoints.iter().find(|e| e.host == file.host).cloned() else {
            self.message =
                "Host unavailable; r retries discovery. Cached logs remain searchable.".into();
            return;
        };
        if !file.present {
            self.message =
                "File is no longer in the host's log directory; showing local cache.".into();
            return;
        }
        let request = self
            .snapshots
            .get(&file.key())
            .map(|s| s.request(true))
            .unwrap_or_else(|| {
                json!({"path":file.path, "offset":-1, "anchor_start":0, "anchor":"", "full":true})
            });
        self.busy = true;
        self.last_bytes = 0;
        self.message = "Downloading full selected file in one query (x cancels)…".into();
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let scope = self.scope_start();
        let handle = rt.spawn(async move {
            let mut accounted = counter.load(Ordering::Relaxed);
            let endpoint = Endpoint {
                host: endpoint.host,
                client: endpoint.client.with_download_counter(counter.clone()),
            };
            let result = collect_since(&endpoint, &directory, &[request], &scope).await;
            let _ = tx.send(CollectorEvent::Report {
                host: endpoint.host.clone(),
                result: result.map_err(|e| format!("{e:#}")),
                bytes: take_delta(&counter, &mut accounted),
            });
            let _ = tx.send(CollectorEvent::Done { auto_tail: false });
        });
        self.abort = Some(handle.abort_handle());
    }

    /// Fetch what is new for the selected file, whether or not it is tracked.
    pub fn download(&mut self, rt: &Runtime) {
        if let Some(key) = self.selected_file().map(LogFile::key) {
            self.sync(rt, Some(key));
        }
    }

    /// Per-host work: every tracked, present file (or just `only`). Error logs
    /// are fetched by time window; others fall back to the eval collector.
    fn plan(&self, only: Option<&str>) -> Vec<HostPlan> {
        let scope = self.scope_start();
        let selected = self.selected_file().map(LogFile::key);
        let mut plans = Vec::new();
        for endpoint in &self.endpoints {
            let mut plan = HostPlan {
                host: endpoint.host.clone(),
                client: Some(endpoint.client.clone()),
                ..HostPlan::default()
            };
            for file in self.files.iter().filter(|f| {
                f.host == endpoint.host
                    && (f.present
                        || (self.is_subscribed(f)
                            && self.snapshots.contains_key(&f.key())
                            && self.is_file_in_history_window(f)))
            }) {
                let key = file.key();
                let snapshot = self.snapshots.get(&key);
                let wanted = match only {
                    Some(only) => only == key,
                    None => {
                        self.is_subscribed(file)
                            || (self.following
                                && snapshot.is_some()
                                && selected.as_deref() == Some(key.as_str()))
                    }
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
        self.busy = true;
        self.last_bytes = 0;
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
                    tasks.spawn(async move {
                        let local = Arc::new(AtomicU64::new(0));
                        let client = client.with_download_counter(local.clone());
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
                            client: client.with_download_counter(local.clone()),
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
                CollectorEvent::Endpoints(endpoints) => self.endpoints = endpoints,
                CollectorEvent::Clock(now) => {
                    if let Ok(now) = DateTime::parse_from_rfc3339(&now) {
                        if self.server_offset != *now.offset() {
                            self.parsed_streams.clear();
                        }
                        self.server_offset = *now.offset();
                        self.clock = Some((now.naive_local(), Instant::now()));
                        self.combined_dirty = self.combined;
                    }
                }
                CollectorEvent::Timed { host, chunk, bytes } => {
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host).or_default() += bytes;
                    if let Err(e) = self.apply_timed(*chunk, at_bottom) {
                        self.message = format!("Cache error: {e:#}");
                    }
                }
                CollectorEvent::Report {
                    host,
                    result,
                    bytes,
                } => {
                    self.body_bytes = self.counter.load(Ordering::Relaxed);
                    self.last_bytes += bytes;
                    *self.host_bytes.entry(host.clone()).or_default() += bytes;
                    match result {
                        Ok(response) => {
                            if let Err(e) = self.apply(response) {
                                self.message = format!("Cache error: {e:#}");
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
                    self.abort = None;
                    self.last_poll = Instant::now();
                    let refresh_queued = self.refresh_queued;
                    self.refresh_queued = false;
                    if refresh_queued {
                        self.refresh(rt);
                    } else if auto_tail && visible {
                        self.sync(rt, None);
                    } else if self.errors.is_empty() {
                        self.message = format!(
                            "Up to date ({}) — {} tracked",
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
        if visible && self.following && !self.busy && self.last_poll.elapsed() >= INTERVAL {
            self.last_poll = Instant::now();
            self.polls = self.polls.wrapping_add(1);
            if self.polls.is_multiple_of(INVENTORY_EVERY) {
                self.refresh(rt);
            } else {
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

    /// Keep `lines`/`matches` in step with an append to the shown file without
    /// reindexing everything already displayed.
    fn extend_view(&mut self, key: &str, fresh: bool, old_len: usize) {
        if fresh {
            self.parsed_streams.remove(key);
        }
        if self.combined {
            self.combined_dirty = true;
            return;
        }
        let selected = self.selected_file().map(LogFile::key);
        if self.all_cached || fresh {
            if self.all_cached || selected.as_deref() == Some(key) {
                self.rebuild_lines();
            }
            return;
        }
        if selected.as_deref() != Some(key) {
            return;
        }
        let Some(snapshot) = self.snapshots.get(key) else {
            return;
        };
        let text = &snapshot.text;
        let from = if old_len > 0 && text.as_bytes()[old_len - 1] != b'\n' {
            text[..old_len].rfind('\n').map_or(0, |i| i + 1)
        } else {
            old_len
        };
        // A partial last line is re-read from its start.
        let keep = self.lines.len() - usize::from(from < old_len);
        self.lines.truncate(keep);
        self.matches.retain(|&index| index < keep);
        self.lines.extend(text[from..].lines().map(str::to_owned));
        let needle = self.search.to_lowercase();
        for index in keep..self.lines.len() {
            if needle.is_empty() || self.lines[index].to_lowercase().contains(&needle) {
                self.matches.push(index);
            }
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
            self.scroll = self.matches.len().saturating_sub(1);
        }
    }

    fn apply(&mut self, response: HostResponse) -> Result<()> {
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        let old_selected = self.selected_file().map(LogFile::key);
        self.read_bytes += response.read_bytes;
        self.cluster_bytes += response.cluster_bytes;
        self.errors.remove(&response.host);
        for file in &mut self.files {
            if response.inventory && file.host == response.host {
                file.present = false;
            }
        }
        for mut file in response.files {
            file.present = true;
            if let Some(existing) = self.files.iter_mut().find(|f| f.key() == file.key()) {
                *existing = file;
            } else {
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
        if !errors.is_empty() {
            self.errors.insert(response.host.clone(), errors.join("; "));
        }
        self.message = if errors.is_empty() {
            format!(
                "{}: {} files in {}{}",
                response.host,
                self.files
                    .iter()
                    .filter(|f| f.host == response.host && f.present)
                    .count(),
                response.directory,
                if cached_chunks > 0 { " — cached" } else { "" }
            )
        } else {
            errors.join("; ")
        };
        Ok(())
    }
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
    let response: HostResponse = serde_json::from_value(endpoint.client.logs_eval(COLLECT,
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
    fn opening_an_empty_cache_waits_for_manual_discovery() {
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

        assert!(!logs.busy);
        assert!(logs.endpoints.is_empty());
        assert_eq!(logs.scope_days, 1);
        assert!(logs.message.contains("Press r to discover"));
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
    fn stale_files_are_hidden_even_when_tracked_and_port_filters_still_cycle() {
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
        assert_eq!(logs.visible_file_indices(), vec![1, 3]);
        logs.selected = 1;
        logs.cycle_selected_port_filter();
        assert_eq!(logs.port_filter_summary(), "+9023 (all except -)");
        logs.toggle_include_only_marked_ports();
        assert_eq!(logs.visible_file_indices(), vec![1]);
        logs.cycle_selected_port_filter();
        assert!(logs.visible_file_indices().is_empty());
        logs.toggle_include_only_marked_ports();
        assert_eq!(logs.visible_file_indices(), vec![3]);
        logs.scope_days = 7;
        assert_eq!(logs.scope_start(), "2026-10-08T00:00:00");
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
                "9060_ErrorLog_1.txt",
                "2026-10-14T01:00:00Z",
            ),
        ];
        logs.sort_files();
        assert_eq!(logs.host_alias("a-host.example"), "h1");
        assert_eq!(logs.host_alias("z-host.example"), "h2");
        assert_eq!(logs.compact_file_label(&logs.files[1]), "9060_E_1");
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
        assert_eq!(logs.visible_file_indices(), vec![0, 2]);
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
        assert_eq!(restored.lines, vec!["old"]);
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
        assert_eq!(restored.lines, vec!["new 🦀"]);
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
        assert!(logs.lines[0].contains("one / ErrorLog.txt"));
        assert!(logs.lines[1].contains("two / ErrorLog.txt"));
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
        assert_eq!(logs.lines, vec!["abc"]);
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
        for host in discovery.hosts {
            let endpoint = Endpoint {
                host: host.clone(),
                client: client.clone(),
            };
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
