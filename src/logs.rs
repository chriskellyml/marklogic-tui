//! Host-scoped query collectors and a persistent, locally searchable log cache.
use crate::client::MarkLogicClient;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as Days, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap, VecDeque},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::mpsc, task::AbortHandle, task::JoinSet};
mod filter;
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
    pub truncation_marker: bool,
}

pub(crate) struct HostView {
    pub host: String,
    pub key: String,
    pub rows: Vec<ViewRow>,
    pub matches: Vec<usize>,
    row_sources: Vec<Option<RowSource>>,
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
    jobs: BTreeMap<(String, String), DownloadProgress>,
}

struct DownloadProgress {
    size: Option<u64>,
    received: Arc<AtomicU64>,
    response_size: Arc<AtomicU64>,
    done: bool,
}

impl LogActivity {
    /// Weighted estimate of work processed, including unchanged and failed jobs.
    fn download_percentage(&self) -> u8 {
        if self.jobs.is_empty() {
            return if self.total == 0 {
                0
            } else {
                ((self.completed.min(self.total) as f64 / self.total as f64) * 100.0) as u8
            };
        }
        let (known_total, known_count) =
            self.jobs
                .values()
                .fold((0.0, 0usize), |(sum, n), job| match job.size {
                    Some(size) if size > 0 => (sum + size as f64, n + 1),
                    _ => (sum, n),
                });
        let unknown_weight = if known_count == 0 {
            1.0
        } else {
            known_total / known_count as f64
        };
        let mut total = 0.0;
        let mut processed = 0.0;
        for job in self.jobs.values() {
            let weight = job.size.map_or(unknown_weight, |size| size.max(1) as f64);
            let fraction = if job.done {
                1.0
            } else {
                let response_size = job.response_size.load(Ordering::Relaxed);
                let denominator = if response_size > 0 {
                    Some(response_size)
                } else {
                    job.size.filter(|size| *size > 0)
                };
                denominator.map_or(0.0, |size| {
                    (job.received.load(Ordering::Relaxed) as f64 / size as f64).min(0.99)
                })
            };
            total += weight;
            processed += weight * fraction;
        }
        ((processed / total) * 100.0).floor().min(100.0) as u8
    }
}

const DISCOVER: &str = include_str!("logs/discover.xqy");
const INVENTORY: &str = include_str!("logs/inventory.xqy");
const COLLECT: &str = include_str!("logs/collect.xqy");
const SCAN: &str = include_str!("logs/scan.xqy");
/// Records per scan round, and rounds per poll, bounding each response.
const SCAN_LIMIT: usize = 20_000;
const SCAN_ROUNDS: usize = 50;
const INTERVAL: Duration = Duration::from_secs(5);
const TRUNCATION_KEY_TIMEOUT: Duration = Duration::from_millis(700);
/// Full responses are buffered, so limit simultaneous large host downloads.
const MAX_CONCURRENT_DOWNLOADS: usize = 2;
/// Large cached logs are read and filtered on a worker rather than parsed on
/// the terminal thread.
const BACKGROUND_PARSE_BYTES: u64 = 1024 * 1024;
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
    /// Newer replacement generations have their own immutable path.
    #[serde(default)]
    generation_path: bool,
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
    fn content_path(&self, root: &Path) -> PathBuf {
        let stem = hash_key(&self.file.key());
        if self.generation_path {
            root.join(format!("{stem}.{}.generation.log", self.generation))
        } else {
            root.join(format!("{stem}.log"))
        }
    }
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
            generation_path: self.generation_path,
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

#[derive(Clone, Copy, PartialEq, Eq)]
struct ParseSpec {
    generation: u64,
    bytes: u64,
    offset: FixedOffset,
    window: TimeWindow,
}

struct ParseJob {
    spec: ParseSpec,
    progress: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
}

impl Drop for ParseJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct ParseResult {
    key: String,
    spec: ParseSpec,
    progress: Arc<AtomicU64>,
    result: std::io::Result<(Stream, bool)>,
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

/// One source's cursor. In-order streams need no per-record merge index.
struct MergeSource<'a> {
    stream: &'a Stream,
    ordered: std::ops::Range<usize>,
    unordered: Vec<usize>,
    cursor: usize,
    window: TimeWindow,
    kinds: [bool; 4],
}

impl MergeSource<'_> {
    fn includes(&self, index: usize) -> bool {
        let record = &self.stream.records[index];
        self.window.contains(record.timestamp)
            && self.kinds[match record.kind {
                Kind::Error => 0,
                Kind::Request => 1,
                Kind::Access => 2,
                Kind::Other => 3,
            }]
    }

    fn next_index(&mut self) -> Option<usize> {
        if !self.unordered.is_empty() {
            let index = *self.unordered.get(self.cursor)?;
            self.cursor += 1;
            return Some(index);
        }
        while let Some(index) = self.ordered.next() {
            if self.includes(index) {
                return Some(index);
            }
        }
        None
    }
}

/// Stable chronological merge. The heap holds at most one record per source;
/// only streams containing late/out-of-order records need a sorted local index.
struct MergedRecords<'a> {
    sources: Vec<MergeSource<'a>>,
    heap: BinaryHeap<Reverse<(Option<NaiveDateTime>, usize, usize)>>,
    first_time: Option<NaiveDateTime>,
}

impl<'a> MergedRecords<'a> {
    fn new(streams: &[&'a Stream], window: TimeWindow, kinds: [bool; 4]) -> Self {
        let mut sources = Vec::with_capacity(streams.len());
        let mut heap = BinaryHeap::new();
        let mut first_time: Option<NaiveDateTime> = None;
        for (source, &stream) in streams.iter().enumerate() {
            let range = stream.window_range(window.start, window.end);
            let mut cursor = MergeSource {
                stream,
                ordered: if stream.is_ordered() {
                    range.clone()
                } else {
                    0..0
                },
                unordered: Vec::new(),
                cursor: 0,
                window,
                kinds,
            };
            if !stream.is_ordered() {
                let indices = range.filter(|&index| cursor.includes(index)).collect();
                cursor.unordered = indices;
                cursor
                    .unordered
                    .sort_by_key(|&index| stream.records[index].timestamp);
            }
            if let Some(index) = cursor.next_index() {
                let first = if stream.is_ordered() {
                    stream.records[index].timestamp
                } else {
                    cursor
                        .unordered
                        .iter()
                        .find_map(|&index| stream.records[index].timestamp)
                };
                if let Some(first) = first {
                    first_time = Some(first_time.map_or(first, |old| old.min(first)));
                }
                heap.push(Reverse((stream.records[index].timestamp, source, index)));
            }
            sources.push(cursor);
        }
        Self {
            sources,
            heap,
            first_time,
        }
    }

    fn first_timestamp(&self) -> Option<NaiveDateTime> {
        self.first_time
    }
}

impl Iterator for MergedRecords<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((_, source, index)) = self.heap.pop()?;
        if let Some(next) = self.sources[source].next_index() {
            self.heap.push(Reverse((
                self.sources[source].stream.records[next].timestamp,
                source,
                next,
            )));
        }
        Some((source, index))
    }
}

pub(crate) struct TruncationDialog {
    pub fields: [String; 3],
    pub selected: usize,
    pub error: String,
}

enum TruncationSequenceKind {
    Prefix,
    Bound {
        start: bool,
        cursor: Option<NaiveDateTime>,
    },
}

struct TruncationSequence {
    kind: TruncationSequenceKind,
    digits: String,
    last_key: Instant,
}

pub(crate) struct LogEntryDetail {
    pub header: String,
    pub raw_text: String,
    pub formatted_text: String,
    pub raw: bool,
    pub wrap: bool,
    pub scroll: u16,
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
    content_filters: filter::State,
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
    /// The truncation window before the most recent committed change.
    undo_window: Option<TimeWindow>,
    default_truncation: bool,
    /// The session window has been initialized, including an explicitly cleared window.
    window_initialized: bool,
    pub truncation_dialog: Option<TruncationDialog>,
    truncation_draft: Option<TruncationDialog>,
    truncation_sequence: Option<TruncationSequence>,
    pub entry_detail: Option<LogEntryDetail>,
    pub view_width: usize,
    pub rows: Vec<ViewRow>,
    pub host_views: Vec<HostView>,
    row_sources: Vec<Option<RowSource>>,
    /// Visible row positions ordered by time for cursor and time navigation.
    time_index: Vec<(NaiveDateTime, usize)>,
    rendered_records: RefCell<VecDeque<RenderedRecord>>,
    first_time: Option<NaiveDateTime>,
    pub mouse_regions: Vec<(Rect, LogMouseTarget)>,
    pub environment_indicator: String,
    pub host_detail_offset: usize,
    pub host_detail_capacity: usize,
    pub host_detail_count: usize,
    pub host_pane_maximized: bool,
    pub show_line_numbers: bool,
    parsed_streams: HashMap<String, Stream>,
    parsed_large: HashMap<String, (ParseSpec, bool)>,
    parse_jobs: HashMap<String, ParseJob>,
    parse_lock: Arc<std::sync::Mutex<()>>,
    parse_tx: mpsc::UnboundedSender<ParseResult>,
    parse_rx: mpsc::UnboundedReceiver<ParseResult>,
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
    day_selection: Option<(String, usize)>,
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
    forced_downloads_queued: BTreeSet<String>,
    merged_downloads_queued: BTreeSet<(String, u32)>,
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
        let (parse_tx, parse_rx) = mpsc::unbounded_channel();
        Self {
            files: Vec::new(),
            selected: 0,
            sidebar_kind: 0,
            files_focus: true,
            file_filter: String::new(),
            file_filter_editing: false,
            search: String::new(),
            search_editing: false,
            content_filters: filter::State::default(),
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
            undo_window: None,
            default_truncation: false,
            window_initialized: false,
            truncation_dialog: None,
            truncation_draft: None,
            truncation_sequence: None,
            entry_detail: None,
            view_width: 0,
            rows: Vec::new(),
            host_views: Vec::new(),
            row_sources: Vec::new(),
            time_index: Vec::new(),
            rendered_records: RefCell::new(VecDeque::new()),
            first_time: None,
            mouse_regions: Vec::new(),
            environment_indicator: String::new(),
            host_detail_offset: 0,
            host_detail_capacity: 0,
            host_detail_count: 0,
            host_pane_maximized: false,
            show_line_numbers: false,
            parsed_streams: HashMap::new(),
            parsed_large: HashMap::new(),
            parse_jobs: HashMap::new(),
            parse_lock: Arc::new(std::sync::Mutex::new(())),
            parse_tx,
            parse_rx,
            combined_dirty: false,
            server_offset: FixedOffset::east_opt(0).unwrap(),
            scope_days: 1,
            view_height: 0,
            extra_streams: BTreeSet::new(),
            disabled_streams: BTreeSet::new(),
            modes: BTreeMap::new(),
            show_untracked: false,
            discovery_days_ago: 0,
            day_selection: None,
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
            forced_downloads_queued: BTreeSet::new(),
            merged_downloads_queued: BTreeSet::new(),
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
                let file = fs::File::open(snapshot.content_path(root))?;
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
        if !self.window_initialized {
            // Initialize once per application session; subsequent visits and
            // connection changes retain the operator's window and cursor.
            self.window = TimeWindow {
                start: Some(chrono::Local::now().naive_local() - Days::minutes(10)),
                end: None,
            };
            self.default_truncation = true;
            self.window_initialized = true;
        }
        if identity != self.identity {
            self.cancel();
            *self = Self {
                opened_from: self.opened_from.take(),
                window: self.window,
                undo_window: self.undo_window,
                default_truncation: self.default_truncation,
                window_initialized: self.window_initialized,
                cursor_time: self.cursor_time,
                scope_days: self.scope_days,
                truncation_dialog: self.truncation_dialog.take(),
                truncation_draft: self.truncation_draft.take(),
                ..Self::default()
            };
            self.identity = identity.clone();
            self.cache_root = report_cache_root(&client, directory);
            self.directory = directory.into();
            if let Err(e) = self.load_cache() {
                self.message = format!("Cache: {e:#}");
            }
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
            jobs: BTreeMap::new(),
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
                "{spinner} {} · ~{}% processed · {} ok, {} failed, {pending} queued · {}{last} · {} received · {}s · x: cancel",
                activity.label,
                activity.download_percentage(),
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
        self.parse_jobs.clear();
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.refresh_queued = false;
        self.demand_queued = false;
        self.demand_files_queued.clear();
        self.forced_downloads_queued.clear();
        self.merged_downloads_queued.clear();
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
        // Applying a completed download above can start a new cache reader.
        self.parse_jobs.clear();
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
                if snapshot.text_bytes > BACKGROUND_PARSE_BYTES {
                    if !fs::metadata(snapshot.content_path(&self.cache_root))
                        .is_ok_and(|metadata| metadata.len() >= snapshot.text_bytes)
                    {
                        continue;
                    }
                } else {
                    let mut bytes = match fs::read(snapshot.content_path(&self.cache_root)) {
                        Ok(bytes) if bytes.len() as u64 >= snapshot.text_bytes => bytes,
                        _ => continue,
                    };
                    bytes.truncate(snapshot.text_bytes as usize);
                    snapshot.text = match String::from_utf8(bytes) {
                        Ok(text) => text,
                        Err(_) => continue,
                    };
                }
            } else {
                snapshot.text_bytes = snapshot.text.len() as u64;
            }
            if snapshot.updated_at.is_none() {
                // Older cache metadata did not record this timestamp. The log
                // file's modification time is the best available approximation.
                snapshot.updated_at = fs::metadata(snapshot.content_path(&self.cache_root))
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

    fn prepare_stream(&mut self, key: &str) -> bool {
        let Some(snapshot) = self.snapshots.get(key) else {
            return false;
        };
        if snapshot.text_bytes <= BACKGROUND_PARSE_BYTES || !snapshot.text.is_empty() {
            self.parsed_streams
                .entry(key.to_owned())
                .or_default()
                .update(&snapshot.file, &snapshot.text, false, self.server_offset);
            return true;
        }
        let spec = ParseSpec {
            generation: snapshot.generation,
            bytes: snapshot.text_bytes,
            offset: self.server_offset,
            window: self.window,
        };
        if self
            .parsed_large
            .get(key)
            .is_some_and(|(ready, _)| *ready == spec)
        {
            return true;
        }
        if self.parse_jobs.get(key).is_some_and(|job| job.spec == spec) {
            return false;
        }
        self.parsed_streams.remove(key);
        self.parsed_large.remove(key);
        self.parse_jobs.remove(key);
        let progress = Arc::new(AtomicU64::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let path = snapshot.content_path(&self.cache_root);
        let file = snapshot.file.clone();
        let tx = self.parse_tx.clone();
        let task_key = key.to_owned();
        let task_progress = progress.clone();
        let task_cancelled = cancelled.clone();
        let lock = self.parse_lock.clone();
        std::thread::spawn(move || {
            let _permit = lock.lock().unwrap_or_else(|error| error.into_inner());
            if task_cancelled.load(Ordering::Relaxed) {
                return;
            }
            let result = fs::File::open(path).and_then(|input| {
                let result = presentation::Stream::read_window(
                    std::io::BufReader::new(input.take(spec.bytes)),
                    &file,
                    spec.offset,
                    spec.window.start,
                    spec.window.end,
                    &task_progress,
                    &task_cancelled,
                )?;
                if task_progress.load(Ordering::Relaxed) < spec.bytes {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "Cached log changed while it was being read",
                    ));
                }
                Ok(result)
            });
            let _ = tx.send(ParseResult {
                key: task_key,
                spec,
                progress: task_progress,
                result,
            });
        });
        self.parse_jobs.insert(
            key.to_owned(),
            ParseJob {
                spec,
                progress,
                cancelled,
            },
        );
        false
    }

    pub fn parsing_status(&self) -> Option<String> {
        if self.parse_jobs.is_empty() {
            return None;
        }
        let total: u128 = self
            .parse_jobs
            .values()
            .map(|job| u128::from(job.spec.bytes))
            .sum();
        let read: u128 = self
            .parse_jobs
            .values()
            .map(|job| u128::from(job.progress.load(Ordering::Relaxed).min(job.spec.bytes)))
            .sum();
        let percent = if total == 0 { 0 } else { read * 100 / total };
        Some(format!(
            "Reading cached logs: {percent}% · {} file(s) · x: cancel",
            self.parse_jobs.len()
        ))
    }

    pub fn large_view_limited(&self) -> bool {
        let selected = self.selected_file();
        self.parsed_large.iter().any(|(key, (spec, limited))| {
            *limited
                && spec.window == self.window
                && self.snapshots.get(key).is_some_and(|snapshot| {
                    selected.is_some_and(|file| {
                        if self.combined {
                            Self::file_port(file) == Self::file_port(&snapshot.file)
                        } else {
                            file.name == snapshot.file.name
                        }
                    })
                })
        })
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
        if mode != TrackingMode::OnDemand {
            self.forced_downloads_queued.remove(&name);
        }
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

    pub fn selected_port(&self) -> Option<&str> {
        self.selected_file().and_then(Self::file_port)
    }
    pub fn cache_is_partial(&self, key: &str) -> bool {
        self.snapshots
            .get(key)
            .is_some_and(|snapshot| snapshot.full_size.is_none())
    }

    pub fn selected_cache_is_partial(&self) -> bool {
        self.selected_file()
            .is_some_and(|file| self.cache_is_partial(&file.key()))
    }

    pub fn local_path(&self) -> Option<PathBuf> {
        let file = self.selected_file()?;
        let snapshot = self.snapshots.get(&file.key())?;
        Some(snapshot.content_path(&self.cache_root))
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

    pub fn merged_host_details(
        &self,
    ) -> Vec<(
        String,
        &str,
        Option<&LogFile>,
        Option<&LogFile>,
        Option<bool>,
        Option<bool>,
    )> {
        let aliases = self.host_aliases();
        let Some(port) = self.selected_port() else {
            return Vec::new();
        };
        self.known_hosts()
            .into_iter()
            .map(|host| {
                let find = |kind| {
                    self.files.iter().find(|file| {
                        file.host == host
                            && Self::file_port(file) == Some(port)
                            && Self::file_days_ago(file) == Some(self.discovery_days_ago)
                            && presentation::source(&file.name).0 == kind
                    })
                };
                let error = find(Kind::Error);
                let access = find(Kind::Access);
                let result = |file: Option<&LogFile>| {
                    file.and_then(|file| {
                        self.download_results
                            .get(&(host.to_owned(), file.name.clone()))
                            .copied()
                    })
                };
                (
                    aliases[host].clone(),
                    host,
                    error,
                    access,
                    result(error),
                    result(access),
                )
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
            if days == 0 {
                self.message = "Already showing today's logs.".into();
            }
            return;
        }
        self.day_selection = self
            .selected_port()
            .map(|port| (port.to_owned(), self.selected_kind_column()));
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

    pub fn selected_display_column(&self) -> usize {
        if self.combined {
            usize::from(self.sidebar_kind == 2)
        } else {
            self.selected_kind_column()
        }
    }

    pub fn merged_modes_by_port(
        &self,
    ) -> BTreeMap<String, (Option<TrackingMode>, Option<TrackingMode>)> {
        let mut modes = BTreeMap::new();
        for file in &self.files {
            if Self::file_days_ago(file) != Some(self.discovery_days_ago) {
                continue;
            }
            let Some(port) = Self::file_port(file) else {
                continue;
            };
            match presentation::source(&file.name).0 {
                Kind::Error => {
                    modes.entry(port.to_owned()).or_insert((None, None)).0 =
                        Some(self.tracking_mode(file))
                }
                Kind::Access => {
                    modes.entry(port.to_owned()).or_insert((None, None)).1 =
                        Some(self.tracking_mode(file))
                }
                _ => {}
            }
        }
        modes
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
        } else if self.combined && preferred_kind < 2 && cells[1 - preferred_kind].is_some() {
            1 - preferred_kind
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

    pub fn select_port_column(&mut self, forwards: bool, include_untracked: bool) {
        self.day_selection = None;
        let rows = self.visible_port_rows();
        let Some(row_index) = self.selected_visible_index() else {
            return;
        };
        let eligible = |index: usize| {
            include_untracked || self.tracking_mode(&self.files[index]) != TrackingMode::Untracked
        };
        let direction: isize = if forwards { 1 } else { -1 };
        let count = if self.combined { 2 } else { 3 };
        let current = self.selected_display_column() as isize;
        for step in 1..count {
            let column = (current + direction * step).rem_euclid(count) as usize;
            let candidate = |cells: &[Option<usize>; 3]| {
                if self.combined && column == 0 {
                    cells[0]
                        .filter(|&index| eligible(index))
                        .or_else(|| cells[1].filter(|&index| eligible(index)))
                } else {
                    cells[if self.combined { 2 } else { column }].filter(|&index| eligible(index))
                }
            };
            // Keep the same port when possible. If that port has no eligible
            // file in this column, select the closest port that does.
            let file_index = candidate(&rows[row_index].1).or_else(|| {
                rows.iter()
                    .enumerate()
                    .filter_map(|(row, (_, cells))| candidate(cells).map(|index| (row, index)))
                    .min_by_key(|(row, _)| (row.abs_diff(row_index), *row))
                    .map(|(_, index)| index)
            });
            if let Some(file_index) = file_index {
                self.select(file_index);
                return;
            }
        }
        self.message = if include_untracked {
            "No other log column is available for this day.".into()
        } else {
            "No other tracked log column is available for this day; use Shift+Left/Right for untracked logs.".into()
        };
    }

    fn ensure_visible_selection(&mut self) {
        if let Some((port, kind)) = self.day_selection.as_ref()
            && let Some(index) = self
                .visible_port_rows()
                .iter()
                .find(|(candidate, _)| candidate == port)
                .and_then(|(_, cells)| cells.get(*kind).copied().flatten())
        {
            self.day_selection = None;
            self.select(index);
            return;
        }
        if self.selected_visible_index().is_none()
            && let Some((_, cells)) = self.visible_port_rows().first()
            && let Some(index) = cells.iter().flatten().next().copied()
        {
            self.select(index);
        }
    }

    pub fn select_visible(&mut self, index: usize) {
        self.day_selection = None;
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

    /// Move to the next tracked file in the selected log-type column.
    pub fn select_tracked_in_column(&mut self, forwards: bool) {
        self.day_selection = None;
        let rows = self.visible_port_rows();
        let current = self.selected_visible_index();
        let column = self.selected_kind_column();
        let eligible = |index: usize| {
            let cells = rows[index].1;
            let tracked = |file_index: &usize| {
                self.files
                    .get(*file_index)
                    .is_some_and(|file| self.tracking_mode(file) != TrackingMode::Untracked)
            };
            if self.combined && column < 2 {
                cells[column]
                    .filter(tracked)
                    .or_else(|| cells[1 - column].filter(tracked))
            } else {
                cells[column].filter(tracked)
            }
        };
        let target = if forwards {
            (current.map_or(0, |row| row.saturating_add(1))..rows.len()).find_map(eligible)
        } else {
            (0..current.unwrap_or(rows.len())).rev().find_map(eligible)
        };
        if let Some(file_index) = target {
            self.select(file_index);
        }
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
        self.message = format!("Collection history: {}.", self.scope_label());
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

    fn truncation_marker(&self, start: bool) -> Option<ViewRow> {
        let time = if start {
            self.window.start?
        } else {
            self.window.end?
        };
        let edge = if start { "before" } else { "after" };
        Some(ViewRow {
            line: Line::styled(
                format!(
                    "── {edge} {time} · log entries truncated ──",
                    time = time.format("%Y-%m-%d %H:%M:%S%.3f")
                ),
                ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
            ),
            timestamp: None,
            error: false,
            start: false,
            search_match: false,
            truncation_marker: true,
        })
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
            self.parse_jobs.clear();
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
                if snapshot.text.is_empty() && snapshot.text_bytes > 0 {
                    let line = Line::from(format!(
                        "[{} / {}] Large cached file: select it to read its entries.",
                        file.host, file.name
                    ));
                    self.lines.push(line.to_string());
                    self.rows.push(ViewRow {
                        line,
                        timestamp: None,
                        error: false,
                        start: false,
                        search_match: false,
                        truncation_marker: true,
                    });
                    self.row_sources.push(None);
                    continue;
                }
                let key: Arc<str> = Arc::from(key.as_str());
                let mut offset = 0;
                let mut previous_time = None;
                let mut has_message = false;
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
                    let starts_message = !has_message
                        || presentation::line_starts_message(
                            &snapshot.file,
                            text,
                            self.server_offset,
                        );
                    has_message |= starts_message;
                    if !self.window.contains(time) {
                        offset += line.len();
                        continue;
                    }
                    self.rows.push(ViewRow {
                        line: Line::default(),
                        timestamp: time,
                        error: false,
                        start: starts_message,
                        search_match: false,
                        truncation_marker: false,
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
            if let Some(marker) = self.truncation_marker(true) {
                self.rows.insert(0, marker);
                self.lines.insert(0, String::new());
                self.row_sources.insert(0, None);
            }
            if let Some(marker) = self.truncation_marker(false) {
                self.rows.push(marker);
                self.lines.push(String::new());
                self.row_sources.push(None);
            }
        } else if let Some(name) = self.selected_file().map(|file| file.name.clone()) {
            let mut files: Vec<_> = self
                .files
                .iter()
                .filter(|file| file.name == name)
                .cloned()
                .collect();
            files.sort_by(|left, right| left.host.cmp(&right.host));
            let wanted: BTreeSet<_> = files.iter().map(LogFile::key).collect();
            self.parse_jobs.retain(|key, _| wanted.contains(key));
            for file in files {
                let key = file.key();
                if !self.snapshots.get(&key).is_some_and(|snapshot| {
                    snapshot.text_bytes > 0 || !snapshot.text.trim().is_empty()
                }) || (file.present && file.size == 0)
                {
                    continue;
                }
                if !self.prepare_stream(&key) {
                    continue;
                }
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
                if let Some(mut marker) = self.truncation_marker(true) {
                    marker.line = Line::from(format!("[{}] {}", file.host, marker.line));
                    rows.push(marker);
                    row_sources.push(None);
                }
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
                            truncation_marker: false,
                        });
                        row_sources.push(Some(RowSource {
                            key: shared_key.clone(),
                            record: record_index,
                            line,
                            first,
                            raw_range: None,
                            wrapped: false,
                        }));
                    }
                }
                if let Some(mut marker) = self.truncation_marker(false) {
                    marker.line = Line::from(format!("[{}] {}", file.host, marker.line));
                    rows.push(marker);
                    row_sources.push(None);
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
                self.row_sources = view.row_sources.clone();
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
            let alias = combined.then(|| self.host_alias(&record.host));
            let mut lines = record.render(&self.options, first, alias.as_deref(), combined);
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
            .and_then(Option::as_ref)
            .map(|source| self.materialize(source, false))
            .unwrap_or_else(|| {
                view.rows
                    .get(index)
                    .map(|row| row.line.clone())
                    .unwrap_or_default()
            })
    }

    pub fn toggle_all_cached(&mut self) {
        if self.combined {
            self.message =
                "Merged view shows cached error and access logs for the selected port.".into();
            return;
        }
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
        let term_filter = filter::Matcher::new(&self.content_filters);
        let leading_key = self.selected_file().map(LogFile::key);
        for view in &mut self.host_views {
            let stream = self.parsed_streams.get(&view.key);
            let mut visible = vec![false; view.rows.len()];
            let mut eligible = vec![true; view.rows.len()];
            let first = view
                .row_sources
                .iter()
                .find_map(|source| source.as_ref().and_then(|source| source.first));
            let mut last_record = None;
            let mut rendered = Vec::new();
            let mut raw_match = false;
            for (index, source) in view.row_sources.iter().enumerate() {
                let Some(source) = source else {
                    visible[index] = true;
                    continue;
                };
                let allowed = stream
                    .and_then(|stream| stream.records.get(source.record))
                    .is_some_and(|record| {
                        term_filter.matches(&[&record.host, &record.file, &record.raw])
                    });
                eligible[index] = allowed;
                let matched = if !allowed {
                    false
                } else if needle.is_empty() {
                    true
                } else if let Some(record) =
                    stream.and_then(|stream| stream.records.get(source.record))
                {
                    if last_record != Some(source.record) {
                        raw_match = record.raw.to_lowercase().contains(&needle);
                        rendered = if raw_match {
                            Vec::new()
                        } else {
                            record.render(&self.options, first, None, false)
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
                .filter_map(|(index, show)| (show && eligible[index]).then_some(index))
                .collect();
            let mut last_time = None;
            view.time_index = view
                .matches
                .iter()
                .enumerate()
                .filter_map(|(position, &row)| {
                    let time = view.rows.get(row)?.timestamp?;
                    if last_time.replace(time) == Some(time) {
                        None
                    } else {
                        Some((time, position))
                    }
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
            self.time_index = view.time_index.clone();
            self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
            if let Some(time) = self.cursor_time {
                self.seek_nearest(time);
            }
            return;
        }
        let eligible: Vec<bool> = if self.all_cached && !self.row_sources.is_empty() {
            let mut eligible = vec![true; self.row_sources.len()];
            let mut i = 0;
            while i < self.row_sources.len() {
                let Some(source) = self.row_sources[i].as_ref() else {
                    i += 1;
                    continue;
                };
                let Some((start, mut end)) = source.raw_range else {
                    i += 1;
                    continue;
                };
                let mut next = i + 1;
                while let Some(Some(other)) = self.row_sources.get(next) {
                    if other.key != source.key || self.rows[next].start {
                        break;
                    }
                    let Some((_, other_end)) = other.raw_range else {
                        break;
                    };
                    end = other_end;
                    next += 1;
                }
                let allowed = self
                    .snapshots
                    .get(source.key.as_ref())
                    .and_then(|snapshot| {
                        snapshot.text.get(start..end).map(|raw| {
                            term_filter.matches(&[&snapshot.file.host, &snapshot.file.name, raw])
                        })
                    })
                    .unwrap_or(true);
                eligible[i..next].fill(allowed);
                i = next;
            }
            eligible
        } else {
            self.lines
                .iter()
                .enumerate()
                .map(|(i, line)| {
                    self.rows.get(i).is_some_and(|row| row.truncation_marker)
                        || term_filter.matches(&[line])
                })
                .collect()
        };
        self.matches = (0..eligible.len())
            .filter(|&i| {
                eligible[i]
                    && (self.rows.get(i).is_some_and(|row| row.truncation_marker)
                        || needle.is_empty()
                        || if self.all_cached {
                            self.display_row(i)
                                .to_string()
                                .to_lowercase()
                                .contains(&needle)
                        } else {
                            self.lines[i].to_lowercase().contains(&needle)
                        })
            })
            .collect();
        if !needle.is_empty() {
            self.expand_context();
            self.matches.retain(|&i| eligible[i]);
        }
        self.rebuild_time_index();
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
        if let Some(time) = self.cursor_time {
            self.seek_nearest(time);
        }
    }

    fn rebuild_time_index(&mut self) {
        let mut last_time = None;
        self.time_index = self
            .matches
            .iter()
            .enumerate()
            .filter_map(|(position, &row)| {
                let time = self.rows.get(row)?.timestamp?;
                if last_time.replace(time) == Some(time) {
                    None
                } else {
                    Some((time, position))
                }
            })
            .collect();
        if !self.time_index.windows(2).all(|pair| pair[0] <= pair[1]) {
            self.time_index.sort_unstable();
        }
    }

    fn nearest_time_position(&self, target: NaiveDateTime) -> Option<usize> {
        let next = self.time_index.partition_point(|(time, _)| *time < target);
        let after = self.time_index.get(next);
        let before = next.checked_sub(1).and_then(|index| {
            let time = self.time_index[index].0;
            // The previous timestamp may occur on many rows. Match the old
            // earliest-row tie behavior without scanning all of them.
            let first = self.time_index.partition_point(|(value, _)| *value < time);
            self.time_index.get(first)
        });
        [before, after]
            .into_iter()
            .flatten()
            .min_by_key(|(time, position)| {
                (
                    (*time - target).num_milliseconds().unsigned_abs(),
                    *position,
                )
            })
            .map(|(_, position)| *position)
    }

    fn expand_context(&mut self) {
        if self.context_before == 0 && self.context_after == 0 {
            return;
        }
        let mut visible = vec![false; self.lines.len()];
        for &index in &self.matches {
            if self
                .rows
                .get(index)
                .is_some_and(|row| row.truncation_marker)
            {
                visible[index] = true;
                continue;
            }
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

    /// Restore the review cursor, or start at the newest line when there is
    /// no cursor yet. Before the first render the pane height is unknown.
    pub fn scroll_to_bottom(&mut self) {
        if let Some(time) = self.cursor_time {
            self.seek_nearest(time);
            return;
        }
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
            && let Some(position) = self.nearest_time_position(target)
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

    /// Restore the initial time window and discard the shared review cursor.
    pub fn reset_time_tracking(&mut self) {
        self.truncation_sequence = None;
        self.scope_days = 1;
        self.window = TimeWindow {
            start: Some(self.now() - Days::minutes(10)),
            end: None,
        };
        self.window_initialized = true;
        self.default_truncation = self.clock.is_none();
        self.undo_window = None;
        self.cursor_time = None;
        self.host_anchor_position = None;
        self.truncation_dialog = None;
        self.truncation_draft = None;
        self.rebuild_lines();
        self.scroll_to_bottom();
        self.message = "Log time reset; truncation starts 10 minutes ago.".into();
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
        if delta == 0 {
            return true;
        }
        let mut starts: Vec<usize> = self
            .matches
            .iter()
            .enumerate()
            .filter_map(|(position, &row)| {
                self.rows
                    .get(row)
                    .is_some_and(|row| row.start && !row.truncation_marker)
                    .then_some(position)
            })
            .collect();
        if starts.is_empty() {
            // A search can leave only continuation/context rows visible.
            // Keep navigation and the shared time cursor usable in that case.
            starts.extend(
                self.matches
                    .iter()
                    .enumerate()
                    .filter_map(|(position, &row)| {
                        self.rows
                            .get(row)
                            .is_some_and(|row| !row.truncation_marker)
                            .then_some(position)
                    }),
            );
        }
        if starts.is_empty() {
            return false;
        }
        let next_start = starts.partition_point(|&position| position <= current);
        let exact_start = starts.binary_search(&current).is_ok();
        let steps = delta.unsigned_abs();
        let target = if delta > 0 {
            next_start.saturating_add(steps.saturating_sub(1))
        } else {
            next_start.saturating_sub(steps.saturating_add(usize::from(exact_start)))
        };
        let position = starts[target.min(starts.len() - 1)];
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
            let position = if end {
                self.matches
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(position, &row)| {
                        self.rows
                            .get(row)
                            .is_some_and(|row| row.start && !row.truncation_marker)
                            .then_some(position)
                    })
                    .or_else(|| {
                        self.matches
                            .iter()
                            .enumerate()
                            .rev()
                            .find_map(|(position, &row)| {
                                self.rows
                                    .get(row)
                                    .is_some_and(|row| !row.truncation_marker)
                                    .then_some(position)
                            })
                    })
                    .unwrap_or(self.matches.len() - 1)
            } else {
                self.matches
                    .iter()
                    .enumerate()
                    .find_map(|(position, &row)| {
                        self.rows
                            .get(row)
                            .is_some_and(|row| row.start && !row.truncation_marker)
                            .then_some(position)
                    })
                    .or_else(|| {
                        self.matches
                            .iter()
                            .enumerate()
                            .find_map(|(position, &row)| {
                                self.rows
                                    .get(row)
                                    .is_some_and(|row| !row.truncation_marker)
                                    .then_some(position)
                            })
                    })
                    .unwrap_or(0)
            };
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
                    position.min(self.matches.len().saturating_sub(self.view_height.max(1)))
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
        self.save_subscriptions();
        let mut failures = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.cache_root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some(SUBSCRIPTIONS_FILE | CATALOG_FILE | filter::PRESETS_FILE)
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
        self.parsed_large.clear();
        self.row_sources.clear();
        self.rendered_records.borrow_mut().clear();
        self.host_views.clear();
        self.rows.clear();
        self.lines.clear();
        self.matches.clear();
        self.all_cached = false;
        self.scroll = 0;
        self.message = if failures.is_empty() {
            "Cleared cached logs; fetching the current history window.".into()
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
        if self.combined && self.sidebar_kind == 2 {
            if let Some(index) = self.selected_visible_index().and_then(|row| {
                self.visible_port_rows()
                    .get(row)
                    .and_then(|(_, cells)| cells[0].or(cells[1]))
            }) {
                self.selected = index;
                self.sidebar_kind = usize::from(Self::log_kind_sort_key(&self.files[index]).min(1));
            } else {
                self.sidebar_kind = 0;
            }
        }
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
                "ACC: {}",
                if self.options.kinds[2] {
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
            5 => self.options.kinds[0] = !self.options.kinds[0],
            6 => self.options.kinds[2] = !self.options.kinds[2],
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
        self.host_views.clear();
        let selected_port = self.selected_port().map(str::to_owned);
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|file| {
                selected_port.as_deref().is_some_and(|port| {
                    Self::file_port(file) == Some(port)
                        && matches!(
                            presentation::source(&file.name).0,
                            Kind::Error | Kind::Access
                        )
                        && self.snapshots.contains_key(&file.key())
                })
            })
            .cloned()
            .collect();
        let wanted: BTreeSet<_> = files.iter().map(LogFile::key).collect();
        self.parse_jobs.retain(|key, _| wanted.contains(key));
        for file in &files {
            let key = file.key();
            self.prepare_stream(&key);
        }
        let host_aliases = self.host_aliases();
        let sources: Vec<_> = files
            .iter()
            .filter_map(|file| {
                self.parsed_streams
                    .get(&file.key())
                    .map(|stream| (Arc::<str>::from(file.key()), stream))
            })
            .collect();
        let records = MergedRecords::new(
            &sources
                .iter()
                .map(|(_, stream)| *stream)
                .collect::<Vec<_>>(),
            self.window,
            self.options.kinds,
        );
        let first = records.first_timestamp();
        self.first_time = first;
        self.rendered_records.borrow_mut().clear();
        let patterns = Patterns::new(&self.search);
        let term_filter = filter::Matcher::new(&self.content_filters);
        let mut eligible = Vec::new();
        self.rows.clear();
        self.row_sources.clear();
        self.lines.clear();
        self.matches.clear();
        if let Some(marker) = self.truncation_marker(true) {
            self.matches.push(self.lines.len());
            self.lines.push(marker.line.to_string());
            self.rows.push(marker);
            self.row_sources.push(None);
            eligible.push(true);
        }
        let mut previous_day = None;
        for (source, record_index) in records {
            let (key, stream) = &sources[source];
            let record = &stream.records[record_index];
            // Search needs the formatted text; ordinary browsing does not.
            // Wrapping needs actual display widths to determine row offsets.
            let rendered = if !self.search.is_empty() || self.options.wrap {
                Some(record.render(
                    &self.options,
                    first,
                    host_aliases.get(&record.host).map(String::as_str),
                    true,
                ))
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
            let allowed = term_filter.matches(&[&record.host, &record.file, &record.raw]);
            let matched =
                allowed && (self.options.search_mode != SearchMode::Filter || search_match);
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
                    truncation_marker: false,
                });
                self.row_sources.push(None);
                eligible.push(true);
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
                            truncation_marker: false,
                        });
                        eligible.push(allowed);
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
                        truncation_marker: false,
                    });
                    eligible.push(allowed);
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
        if let Some(marker) = self.truncation_marker(false) {
            self.matches.push(self.lines.len());
            self.lines.push(marker.line.to_string());
            self.rows.push(marker);
            self.row_sources.push(None);
        }
        if !self.search.is_empty() && self.options.search_mode == SearchMode::Filter {
            self.expand_context();
            self.matches
                .retain(|&i| eligible.get(i).copied().unwrap_or(true));
        }
        self.rebuild_time_index();
        self.scroll = self.scroll.min(self.matches.len().saturating_sub(1));
    }

    /// Timestamp at the visible cursor: the highlighted host cursor in split
    /// view, otherwise the first visible line (also highlighted by the UI).
    pub fn current_line_time(&self) -> Option<NaiveDateTime> {
        let index = *self.matches.get(self.anchor_position()?)?;
        self.rows
            .get(index)
            .and_then(|row| row.timestamp)
            .or_else(|| {
                let source = self.row_sources.get(index)?.as_ref()?;
                let (start, end) = source.raw_range?;
                let snapshot = self.snapshots.get(source.key.as_ref())?;
                let current = snapshot.text.get(start..end)?;
                presentation::line_time(&snapshot.file, current, self.server_offset).or_else(|| {
                    snapshot.text.get(..start)?.lines().rev().find_map(|line| {
                        presentation::line_time(&snapshot.file, line, self.server_offset)
                    })
                })
            })
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
        self.set_truncation_at_time(start, time);
    }

    fn set_truncation_at_time(&mut self, start: bool, time: NaiveDateTime) {
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
        let anchor = self.ensure_cursor_time();
        self.apply_window(window, anchor);
    }

    /// Recognize a short sequence without delaying unrelated keys or input fields.
    pub fn handle_truncation_sequence(&mut self, key: crossterm::event::KeyEvent) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        self.expire_truncation_sequence();
        let digit = match key.code {
            KeyCode::Char(c @ '0'..='9') if key.modifiers.is_empty() => Some(c),
            _ => None,
        };
        let bound = match key.code {
            KeyCode::Char('t') if key.modifiers.is_empty() => Some(true),
            KeyCode::Char('T')
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                Some(false)
            }
            KeyCode::Char('t') if key.modifiers == KeyModifiers::SHIFT => Some(false),
            _ => None,
        };
        if let Some(mut sequence) = self.truncation_sequence.take() {
            if let Some(digit) = digit {
                sequence.digits.push(digit);
                sequence.last_key = Instant::now();
                self.message = format!(
                    "Truncation shortcut: {}{}",
                    match sequence.kind {
                        TruncationSequenceKind::Prefix => "",
                        TruncationSequenceKind::Bound { start: true, .. } => "t",
                        TruncationSequenceKind::Bound { start: false, .. } => "T",
                    },
                    sequence.digits
                );
                self.truncation_sequence = Some(sequence);
                return true;
            }
            match (&sequence.kind, bound) {
                (TruncationSequenceKind::Prefix, Some(start)) => {
                    self.adjust_truncation_by_minutes(
                        start,
                        false,
                        &sequence.digits,
                        self.current_line_time(),
                    );
                    return true;
                }
                (
                    TruncationSequenceKind::Bound {
                        start: true,
                        cursor,
                    },
                    Some(true),
                ) if sequence.digits.is_empty() => {
                    self.center_truncation(*cursor);
                    return true;
                }
                _ => self.finish_truncation_sequence(sequence),
            }
        }
        if self.files_focus {
            return false;
        }
        let kind = if let Some(start) = bound {
            TruncationSequenceKind::Bound {
                start,
                cursor: self.current_line_time(),
            }
        } else if digit.is_some() {
            TruncationSequenceKind::Prefix
        } else {
            return false;
        };
        let digits = digit.map_or_else(String::new, |digit| digit.to_string());
        self.message = if let Some(start) = bound {
            format!("Truncation shortcut: {}…", if start { "t" } else { "T" })
        } else {
            format!("Truncation shortcut: {digits}…")
        };
        self.truncation_sequence = Some(TruncationSequence {
            kind,
            digits,
            last_key: Instant::now(),
        });
        true
    }

    fn finish_truncation_sequence(&mut self, sequence: TruncationSequence) {
        match sequence.kind {
            TruncationSequenceKind::Prefix => {
                self.message = "Enter t or T after the number to adjust a truncation bound.".into();
            }
            TruncationSequenceKind::Bound { start, cursor } if sequence.digits.is_empty() => {
                if let Some(time) = cursor {
                    self.set_truncation_at_time(start, time);
                } else {
                    self.message =
                        "The current line has no timestamp; select a timestamped log line.".into();
                }
            }
            TruncationSequenceKind::Bound { start, cursor } => {
                self.adjust_truncation_by_minutes(start, true, &sequence.digits, cursor);
            }
        }
    }

    fn expire_truncation_sequence(&mut self) {
        if self
            .truncation_sequence
            .as_ref()
            .is_some_and(|sequence| sequence.last_key.elapsed() >= TRUNCATION_KEY_TIMEOUT)
            && let Some(sequence) = self.truncation_sequence.take()
        {
            self.finish_truncation_sequence(sequence);
        }
    }

    fn adjust_truncation_by_minutes(
        &mut self,
        start: bool,
        forwards: bool,
        digits: &str,
        cursor: Option<NaiveDateTime>,
    ) {
        let Some(delta) = digits.parse::<i64>().ok().and_then(Days::try_minutes) else {
            self.message = "Truncation adjustment is too large.".into();
            return;
        };
        let base = if start {
            self.window.start
        } else {
            self.window.end
        }
        .or(cursor);
        let Some(base) = base else {
            self.message = "Set this truncation bound or select a timestamped line first.".into();
            return;
        };
        let time = if forwards {
            base.checked_add_signed(delta)
        } else {
            base.checked_sub_signed(delta)
        };
        if let Some(time) = time {
            self.set_truncation_at_time(start, time);
        } else {
            self.message = "Adjusted timestamp is out of range.".into();
        }
    }

    fn center_truncation(&mut self, cursor: Option<NaiveDateTime>) {
        let Some(time) = cursor else {
            self.message =
                "The current line has no timestamp; select a timestamped log line.".into();
            return;
        };
        let half = Days::seconds(150);
        let (Some(start), Some(end)) =
            (time.checked_sub_signed(half), time.checked_add_signed(half))
        else {
            self.message = "Centered window is out of range.".into();
            return;
        };
        self.cursor_time = Some(time);
        self.apply_window(
            TimeWindow {
                start: Some(start),
                end: Some(end),
            },
            Some(time),
        );
    }

    fn apply_window(&mut self, window: TimeWindow, anchor: Option<NaiveDateTime>) {
        if window != self.window {
            self.undo_window = Some(self.window);
        }
        self.install_window(window, anchor);
    }

    fn install_window(&mut self, window: TimeWindow, anchor: Option<NaiveDateTime>) {
        self.window = window;
        self.default_truncation = false;
        self.window_initialized = true;
        self.truncation_draft = None;
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

    /// Undo the most recent committed truncation-window change.
    pub fn undo_truncation(&mut self) {
        let Some(window) = self.undo_window.take() else {
            self.message = "No truncation change to undo.".into();
            return;
        };
        let anchor = self.ensure_cursor_time();
        self.install_window(window, anchor);
        self.message = "Last truncation change undone.".into();
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
        if let Some(draft) = self.truncation_draft.take() {
            self.truncation_dialog = Some(draft);
            return;
        }
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

    pub fn close_truncation_dialog(&mut self) {
        if let Some(dialog) = self.truncation_dialog.take() {
            self.truncation_draft = Some(dialog);
        }
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
        let anchor = self.ensure_cursor_time();
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
                self.time_editing = false;
            }
            None => self.message = "Use HH:MM:SS or YYYY-MM-DD HH:MM:SS (server time).".into(),
        }
    }
    fn seek_nearest(&mut self, target: NaiveDateTime) {
        if let Some(position) = self.nearest_time_position(target) {
            self.host_anchor_position = Some(position);
            self.scroll = if !self.combined && !self.all_cached && self.host_views.len() > 1 {
                position.saturating_sub(self.view_height.max(1) / 2)
            } else {
                position.min(self.matches.len().saturating_sub(self.view_height.max(1)))
            };
        }
    }
    pub fn jump_time(&mut self, seconds: i64) {
        if let Some(time) = self.anchor_timestamp() {
            let target = time + Days::seconds(seconds);
            self.cursor_time = Some(target);
            self.host_anchor_position = None;
            self.seek_nearest(target);
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

    fn current_entry_detail(&self) -> Option<LogEntryDetail> {
        let cursor = self.anchor_position()?;
        let row = *self.matches.get(cursor)?;
        let source = self.row_sources.get(row)?.as_ref()?;
        let snapshot = self.snapshots.get(source.key.as_ref())?;

        let (raw_text, formatted_text, header) = if let Some((target, _)) = source.raw_range {
            let mut message_start = 0;
            let mut message_end = snapshot.text.len();
            let mut has_message = false;
            let mut offset = 0;
            for line in snapshot.text.split_inclusive('\n') {
                let text = line.trim_end_matches(['\r', '\n']);
                let starts = !has_message
                    || presentation::line_starts_message(&snapshot.file, text, self.server_offset);
                if starts {
                    if offset > target {
                        message_end = offset;
                        break;
                    }
                    message_start = offset;
                    has_message = true;
                }
                offset += line.len();
            }
            let raw = snapshot
                .text
                .get(message_start..message_end)?
                .trim_end_matches(['\r', '\n'])
                .to_owned();
            let header = format!("{}  {}", snapshot.file.host, snapshot.file.name);
            (raw.clone(), raw, header)
        } else {
            let record = self
                .parsed_streams
                .get(source.key.as_ref())?
                .records
                .get(source.record)?;
            let raw = record.raw.clone();
            let alias = self.combined.then(|| self.host_alias(&record.host));
            let formatted = record
                .render(&self.options, source.first, alias.as_deref(), self.combined)
                .into_iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let timestamp = record
                .timestamp
                .map(|time| time.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
                .unwrap_or_else(|| "untimed".into());
            let header = format!("{}  {}  {}", record.host, record.file, timestamp);
            (raw, formatted, header)
        };

        Some(LogEntryDetail {
            header,
            raw_text,
            formatted_text,
            raw: self.options.raw,
            wrap: true,
            scroll: 0,
        })
    }

    pub fn open_entry_detail(&mut self) {
        self.entry_detail = self.current_entry_detail();
        if self.entry_detail.is_none() {
            self.message = "Select a log entry to view it in detail.".into();
        }
    }

    pub fn entry_detail_export(&self) -> Result<PathBuf> {
        let detail = self
            .entry_detail
            .as_ref()
            .context("No log entry detail is open")?;
        let path = crate::external_editor::temp_editor_path(Some(Path::new("log-entry.txt")));
        let body = if detail.raw {
            &detail.raw_text
        } else {
            &detail.formatted_text
        };
        fs::write(&path, format!("{}\n\n{}\n", detail.header, body))?;
        Ok(path)
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
            && !self.content_filter_active()
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
        self.discovery_revision = self.discovery_revision.wrapping_add(1);
        self.ensure_visible_selection();
        self.refresh(rt);
    }

    /// Download every available host's copy of the selected file in full.
    pub fn download_full(&mut self, rt: &Runtime) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        if matches!(
            self.tracking_mode(&file),
            TrackingMode::Untracked | TrackingMode::Partial
        ) {
            self.set_selected_mode(file.clone(), TrackingMode::OnDemand);
        }
        if self.busy {
            self.forced_downloads_queued.insert(file.name.clone());
            self.message = format!("Full download queued: {}.", file.name);
            return;
        }
        self.start_full_download(rt, &file.name);
    }

    fn start_full_download(&mut self, rt: &Runtime, name: &str) {
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for source in self.files.iter().filter(|source| source.name == name) {
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

    /// Download the selected port's error and access files for the selected day.
    pub fn download_merged(&mut self, rt: &Runtime) {
        let Some(port) = self.selected_port().map(str::to_owned) else {
            self.message = "Select a port to download its merged logs.".into();
            return;
        };
        let day = self.discovery_days_ago;
        if self.busy {
            self.merged_downloads_queued.insert((port, day));
            self.message = "Merged log download queued.".into();
            return;
        }
        self.start_merged_download(rt, &port, day);
    }

    fn merged_download_plans(&self, port: &str, days_ago: u32) -> Vec<(Endpoint, Vec<Value>)> {
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for file in &self.files {
            if Self::file_port(file) != Some(port)
                || Self::file_days_ago(file) != Some(days_ago)
                || !matches!(
                    presentation::source(&file.name).0,
                    Kind::Error | Kind::Access
                )
            {
                continue;
            }
            let request = self.snapshots.get(&file.key()).map(|snapshot| snapshot.request(true))
                .unwrap_or_else(|| json!({"path":file.path, "offset":-1, "anchor_start":0, "anchor":"", "full":true}));
            requests.entry(file.host.clone()).or_default().push(request);
        }
        self.endpoints
            .iter()
            .filter_map(|endpoint| {
                requests
                    .remove(&endpoint.host)
                    .map(|requests| (endpoint.clone(), requests))
            })
            .collect()
    }

    fn start_merged_download(&mut self, rt: &Runtime, port: &str, days_ago: u32) {
        let plans = self.merged_download_plans(port, days_ago);
        if plans.is_empty() {
            self.message =
                format!("No error or access logs available for port {port} on the selected day.");
            return;
        }
        self.start_downloads(rt, plans, "Downloading merged logs");
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
                    .map(|file| file.size);
                jobs.push((
                    size,
                    endpoint.clone(),
                    name,
                    request,
                    Arc::new(AtomicU64::new(0)),
                    Arc::new(AtomicU64::new(0)),
                ));
            }
        }
        // Finish small files promptly rather than making them wait for a large
        // file included in the same host response.
        jobs.sort_by_key(|job| job.0.unwrap_or(u64::MAX));
        let progress = self.begin_activity(label, jobs.len());
        let activity = self.activity.as_mut().unwrap();
        activity.download = true;
        for (size, endpoint, file, _, received, response_size) in &jobs {
            activity.jobs.insert(
                (endpoint.host.clone(), file.clone()),
                DownloadProgress {
                    size: *size,
                    received: received.clone(),
                    response_size: response_size.clone(),
                    done: false,
                },
            );
        }
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let handle = rt.spawn(async move {
            let mut tasks = JoinSet::new();
            let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
            for (_, endpoint, file, request, local, response_size) in jobs {
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
                    let endpoint = Endpoint {
                        host: endpoint.host,
                        client: endpoint
                            .client
                            .with_download_counter(local.clone())
                            .with_download_progress(progress)
                            .with_download_response_size(response_size),
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
        self.expire_truncation_sequence();
        self.tick_filter();
        // Content pinned to the bottom stays pinned; a user who scrolled up
        // is not yanked back by background updates.
        let at_bottom = self.scroll + self.view_height.max(1) >= self.matches.len();
        // Return to input/rendering between batches even if collectors keep
        // producing completions. A single large completion can still take
        // longer; streaming ingestion is needed to remove that cost.
        for _ in 0..16 {
            let Ok(event) = self.rx.try_recv() else { break };
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
                        if let Some(job) = activity.jobs.get_mut(&(host.clone(), file.clone())) {
                            job.done = true;
                        }
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
                            self.window.start = Some(now.naive_local() - Days::minutes(10));
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
                    if !refresh_queued
                        && finished
                            .as_ref()
                            .is_some_and(|activity| activity.label == "Discovering logs")
                    {
                        self.day_selection = None;
                    }
                    if refresh_queued {
                        self.refresh(rt);
                    }
                    while !self.busy {
                        let Some(name) = self.forced_downloads_queued.pop_first() else {
                            break;
                        };
                        if matches!(
                            self.mode_for_name(&name),
                            TrackingMode::OnDemand | TrackingMode::Live
                        ) {
                            self.start_full_download(rt, &name);
                        }
                    }
                    while !self.busy {
                        let Some((port, days_ago)) = self.merged_downloads_queued.pop_first()
                        else {
                            break;
                        };
                        self.start_merged_download(rt, &port, days_ago);
                    }
                    if !self.busy && self.demand_queued {
                        self.demand_queued = false;
                        self.demand_files_queued.clear();
                        self.sync_on_demand(rt, None);
                    }
                    while !self.busy {
                        let Some(key) = self.demand_files_queued.pop_first() else {
                            break;
                        };
                        self.sync_on_demand(rt, Some(key));
                    }
                    if !self.busy
                        && auto_tail
                        && visible
                        && self
                            .files
                            .iter()
                            .any(|file| self.tracking_mode(file) == TrackingMode::Live)
                    {
                        self.sync(rt, None);
                    }
                    if self.busy {
                        // A queued operation now owns the activity display.
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
        let mut parsed = false;
        for _ in 0..4 {
            let Ok(result) = self.parse_rx.try_recv() else {
                break;
            };
            let current = self.parse_jobs.get(&result.key).is_some_and(|job| {
                job.spec == result.spec && Arc::ptr_eq(&job.progress, &result.progress)
            });
            if !current {
                continue;
            }
            self.parse_jobs.remove(&result.key);
            match result.result {
                Ok((stream, truncated)) => {
                    self.parsed_streams.insert(result.key.clone(), stream);
                    self.parsed_large
                        .insert(result.key, (result.spec, truncated));
                    if truncated {
                        self.message = "Large log view limited to recent records; narrow the time window to see earlier entries.".into();
                    }
                    parsed = true;
                }
                Err(error) if error.kind() != std::io::ErrorKind::Interrupted => {
                    self.message = format!("Reading cached log: {error}");
                }
                Err(_) => {}
            }
        }
        if parsed {
            self.rebuild_lines();
            if let Some(time) = self.cursor_time {
                self.seek_nearest(time);
            } else if self.following {
                self.scroll_to_bottom();
            }
        }
        self.body_bytes = self.counter.load(Ordering::Relaxed);
        // A scan can deliver many chunks from many streams in one event-loop
        // pass. Render that batch once, retaining an operator's review time
        // when older records arrive out of order.
        if self.combined && self.combined_dirty {
            let reviewing = self.anchor_timestamp();
            self.rebuild_combined();
            if let Some(time) = reviewing {
                self.seek_nearest(time);
            } else if self.following && at_bottom {
                self.scroll_to_bottom();
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
            Some(prior) if !fresh => (prior.meta(), prior.text_bytes as usize),
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
                    generation_path: prior_generation.is_some(),
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
        let log = meta.content_path(&self.cache_root);
        let metadata_path = self.cache_root.join(format!("{stem}.json"));
        if fresh {
            // Write a replacement beside the active generation. The manifest
            // switches only after all new bytes are present, so old readers and
            // a restarted process can continue using the prior generation.
            let stage = self
                .cache_root
                .join(format!("{stem}.{}.staging", meta.generation));
            let result = (|| -> Result<()> {
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&stage)?;
                out.write_all(text.as_bytes())?;
                out.sync_all()?;
                drop(out);
                fs::rename(&stage, &log)?;
                if fresh || !text.is_empty() {
                    meta.updated_at = Some(Utc::now().to_rfc3339());
                }
                atomic_write(&metadata_path, &serde_json::to_vec(&meta)?)?;
                Ok(())
            })();
            if let Err(error) = result {
                let _ = fs::remove_file(&stage);
                // Never delete the previous active generation on failure.
                if prior_generation.is_some() {
                    let _ = fs::remove_file(&log);
                }
                return Err(error);
            }
            if let Some(previous) = prior {
                // The compatibility name makes rotated generations visible to
                // existing tools while keeping the old reader path intact.
                let old = previous.content_path(&self.cache_root);
                let alias = self
                    .cache_root
                    .join(format!("{stem}.{}.previous.log", previous.generation));
                let _ = fs::hard_link(old, alias);
            }
        } else {
            // An interrupted append can leave bytes beyond the published cursor.
            // Remove that uncommitted suffix before appending the next batch.
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .open(&log)
                .with_context(|| format!("Opening {}", log.display()))?;
            let actual = out.metadata()?.len();
            if actual < base as u64 {
                bail!("Cached log is shorter than its committed size");
            }
            out.set_len(base as u64)?;
            use std::io::{Seek, SeekFrom};
            out.seek(SeekFrom::Start(base as u64))?;
            out.write_all(text.as_bytes())?;
            if !text.is_empty() {
                meta.updated_at = Some(Utc::now().to_rfc3339());
            }
            atomic_write(&metadata_path, &serde_json::to_vec(&meta)?)?;
        }
        if fresh {
            self.parsed_streams.remove(&key);
            self.parsed_large.remove(&key);
            self.parse_jobs.remove(&key);
        }
        if meta.text_bytes > BACKGROUND_PARSE_BYTES {
            self.snapshots.insert(key, meta);
        } else {
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
            generation_path: previous.is_some(),
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
    fn opening_an_empty_cache_preserves_scope_and_starts_discovery() {
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
        assert_eq!(logs.scope_days, 30);
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
    fn file_navigation_stays_in_the_tracked_column_or_moves_one_row_with_shift() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        for (name, mode) in [
            ("8000_ErrorLog.txt", TrackingMode::OnDemand),
            ("8001_AccessLog.txt", TrackingMode::OnDemand),
            ("8002_ErrorLog.txt", TrackingMode::Untracked),
            ("8003_ErrorLog.txt", TrackingMode::Live),
            ("8004_RequestLog.txt", TrackingMode::OnDemand),
            ("8005_ErrorLog.txt", TrackingMode::Partial),
        ] {
            app.server_logs
                .files
                .push(named_file("one", name, "2026-10-14T12:00:00Z"));
            app.server_logs.modes.insert(name.into(), mode);
        }
        app.server_logs.show_untracked = true;
        app.server_logs.files_focus = true;
        app.server_logs.select(0);
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
        };
        send(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8003_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8005_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8005_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8003_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8002_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('K'), KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8001_AccessLog.txt"
        );
        send(&mut app, KeyCode::Char('k'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8001_AccessLog.txt"
        );
        send(&mut app, KeyCode::Char('j'), KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8002_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('J'), KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8003_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8004_RequestLog.txt"
        );
        send(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8004_RequestLog.txt"
        );
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
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
    fn review_time_and_bounds_survive_log_switches_and_view_resets() {
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:05:00.000 Info: middle\n2026-10-14 08:10:00.000 Info: last\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        let other = named_file("one", "TaskServer_ErrorLog.txt", "2026-10-14T08:10:00Z");
        logs.files.push(other.clone());
        logs.commit(
            other,
            true,
            "2026-10-14 08:04:00.000 Info: earlier\n2026-10-14 08:06:00.000 Info: nearest\n2026-10-14 08:10:00.000 Info: later\n",
            |_| {},
        )
        .unwrap();
        let offset = FixedOffset::east_opt(0).unwrap();
        let parse = |text| presentation::parse_time(text, offset).unwrap();
        let window = TimeWindow {
            start: Some(parse("2026-10-14 08:00:00")),
            end: Some(parse("2026-10-14 08:10:00")),
        };
        logs.apply_window(window, None);
        logs.view_height = 1;
        logs.time_input = "2026-10-14 08:05:30".into();
        logs.seek_time();
        let cursor = Some(parse("2026-10-14 08:05:30"));
        let index = logs
            .files
            .iter()
            .position(|f| f.name == "TaskServer_ErrorLog.txt")
            .unwrap();
        logs.select(index);
        assert_eq!(logs.cursor_time, cursor);
        assert_eq!(logs.current_line_time(), Some(parse("2026-10-14 08:06:00")));
        logs.open_truncation_dialog();
        logs.apply_truncation_dialog();
        logs.scroll_to_bottom();
        assert_eq!(logs.cursor_time, cursor);
        assert_eq!(logs.window, window);
        assert_eq!(logs.current_line_time(), Some(parse("2026-10-14 08:06:00")));
        assert_eq!(logs.scroll, logs.anchor_position().unwrap());
        logs.toggle_all_cached();
        logs.toggle_combined();
        assert_eq!(logs.cursor_time, cursor);
        assert_eq!(logs.window, window);
        logs.files_focus = false;
        logs.set_truncation_at_cursor(true);
        assert_eq!(logs.cursor_time, cursor);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn double_escape_and_screen_return_keep_time_and_bounds() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.server_logs = viewer();
        let offset = FixedOffset::east_opt(0).unwrap();
        let cursor = presentation::parse_time("2026-10-14 08:05:30", offset);
        let window = TimeWindow {
            start: presentation::parse_time("2026-10-14 08:00:00", offset),
            end: presentation::parse_time("2026-10-14 08:10:00", offset),
        };
        app.server_logs.apply_window(window, None);
        app.server_logs.cursor_time = cursor;
        app.mode = crate::AppMode::MarkLogicLogs;
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        crate::events::handle_key_event(&mut app, escape).unwrap();
        crate::events::handle_key_event(&mut app, escape).unwrap();
        assert_eq!(app.mode, crate::AppMode::StartPage);
        app.switch_screen(crate::Screen::Logs);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn keyboard_navigation_and_escape_preserve_truncation_across_log_types() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        let entries = [
            (
                "8000_ErrorLog.txt",
                "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:05:00.000 Info: selected\n2026-10-14 08:10:00.000 Info: last\n",
            ),
            (
                "8000_AccessLog.txt",
                "127.0.0.1 - user [14/Oct/2026:08:04:30 +0000] \"GET /a HTTP/1.1\" 200 1\n127.0.0.1 - user [14/Oct/2026:08:07:00 +0000] \"GET /b HTTP/1.1\" 200 1\n",
            ),
            (
                "8000_RequestLog.txt",
                "{\"time\":\"2026-10-14T08:06:50Z\",\"url\":\"/a\"}\n{\"time\":\"2026-10-14T08:09:00Z\",\"url\":\"/b\"}\n",
            ),
        ];
        for (name, text) in entries {
            let file = named_file("one", name, "2026-10-14T09:00:00Z");
            logs.files.push(file.clone());
            logs.modes.insert(name.into(), TrackingMode::OnDemand);
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select(0);
        logs.files_focus = false;
        logs.view_height = 1;
        let client = test_client();
        logs.identity = format!(
            "{}\0{}\0{}\0",
            client.server.name,
            client.server.base_url(),
            client.server.username
        );
        logs.client = Some(client.clone());
        let mut app = crate::tests::test_app();
        app.client = Some(client);
        app.server_logs = logs;
        app.mode = crate::AppMode::MarkLogicLogs;
        let key = |code, modifiers| KeyEvent::new(code, modifiers);
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, key(code, modifiers)).unwrap();
        };
        let offset = FixedOffset::east_opt(0).unwrap();
        let time = |value| presentation::parse_time(value, offset).unwrap();

        send(&mut app, KeyCode::Char('g'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.cursor_time,
            Some(time("2026-10-14 08:05:00"))
        );
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        send(&mut app, KeyCode::Right, KeyModifiers::NONE);
        send(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_AccessLog.txt"
        );
        assert_eq!(
            app.server_logs.cursor_time,
            Some(time("2026-10-14 08:05:00"))
        );
        assert_eq!(
            app.server_logs.current_line_time(),
            Some(time("2026-10-14 08:04:30"))
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 30)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Time: 2026-10-14 08:05:00.000"));
        assert!(screen.contains("08:04:30.000"));
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.cursor_time,
            Some(time("2026-10-14 08:07:00"))
        );
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        send(&mut app, KeyCode::Right, KeyModifiers::NONE);
        send(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_RequestLog.txt"
        );
        assert_eq!(
            app.server_logs.cursor_time,
            Some(time("2026-10-14 08:07:00"))
        );
        assert_eq!(
            app.server_logs.current_line_time(),
            Some(time("2026-10-14 08:06:50"))
        );
        send(&mut app, KeyCode::Char('t'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('T'), KeyModifiers::SHIFT);
        let sequence = app.server_logs.truncation_sequence.take().unwrap();
        app.server_logs.finish_truncation_sequence(sequence);
        let window = app.server_logs.window;
        let cursor = app.server_logs.cursor_time;
        assert_eq!(window.start, Some(time("2026-10-14 08:06:50")));
        assert_eq!(window.end, Some(time("2026-10-14 08:09:00")));

        send(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_RequestLog.txt"
        );
        send(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.mode, crate::AppMode::StartPage);
        app.command_input = ":logs".into();
        app.execute_command();
        assert_eq!(app.mode, crate::AppMode::MarkLogicLogs);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        assert_eq!(
            app.server_logs.current_line_time(),
            Some(time("2026-10-14 08:09:00"))
        );
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Time: 2026-10-14 08:09:00.000"));
        assert!(screen.contains("2026-10-14 08:06:50.000"));
        assert!(screen.contains("2026-10-14 08:09:00.000"));

        send(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let dialog = app.server_logs.truncation_dialog.as_ref().unwrap();
        assert_eq!(dialog.fields[0], "2026-10-14 08:06:50.000");
        assert_eq!(dialog.fields[1], "2026-10-14 08:09:00.000");
        send(&mut app, KeyCode::Char('u'), KeyModifiers::CONTROL);
        for c in "2026-10-14 08:06:55".chars() {
            send(&mut app, KeyCode::Char(c), KeyModifiers::NONE);
        }
        send(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.server_logs.window, window);
        send(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.mode, crate::AppMode::StartPage);
        app.command_input = ":logs".into();
        app.execute_command();
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        send(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        assert_eq!(
            app.server_logs.truncation_dialog.as_ref().unwrap().fields[0],
            "2026-10-14 08:06:55"
        );
        send(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.window.start,
            Some(time("2026-10-14 08:06:55"))
        );
        assert_eq!(app.server_logs.cursor_time, cursor);
        app.server_logs.cancel();
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn reopening_a_different_log_context_retains_session_time_and_bounds() {
        let mut logs = viewer();
        let original_cache = logs.cache_root.clone();
        let offset = FixedOffset::east_opt(0).unwrap();
        let cursor = presentation::parse_time("2026-10-14 08:05:30", offset);
        let window = TimeWindow {
            start: presentation::parse_time("2026-10-14 08:00:00", offset),
            end: presentation::parse_time("2026-10-14 08:10:00", offset),
        };
        logs.apply_window(window, None);
        logs.cursor_time = cursor;
        logs.scope_days = 7;
        let runtime = Runtime::new().unwrap();
        let client = test_client();
        let directory = original_cache.to_string_lossy().into_owned();
        logs.open(Some(client), &directory, crate::AppMode::Normal, &runtime);
        assert_eq!(logs.window, window);
        assert_eq!(logs.cursor_time, cursor);
        assert_eq!(logs.scope_days, 7);
        logs.cancel();
        if logs.cache_root.exists() {
            fs::remove_dir_all(&logs.cache_root).unwrap();
        }
        fs::remove_dir_all(original_cache).unwrap();
    }

    #[test]
    fn large_cached_file_loads_in_background_and_shows_its_window() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let text = format!(
            "2026-10-14 08:00:00.000 Info: {}\n2026-10-14 09:00:00.000 Error: recent\n",
            "x".repeat(BACKGROUND_PARSE_BYTES as usize)
        );
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        let other = named_file("one", "8000_AccessLog.txt", "2026-10-14T09:00:00Z");
        logs.files.push(other.clone());
        logs.commit(other, true, "small log\n", |_| {}).unwrap();
        logs.window.start = presentation::parse_time("2026-10-14 08:50:00", logs.server_offset);
        logs.commit(source.clone(), true, &text, |_| {}).unwrap();
        assert!(logs.snapshots[&source.key()].text.is_empty());
        let started = Instant::now();
        logs.select(0);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "selection blocked on parsing"
        );
        assert!(logs.parsing_status().is_some());
        logs.select(1);
        assert!(
            logs.parse_jobs.is_empty(),
            "leaving a file cancels its reader"
        );
        assert_eq!(logs.selected_file().unwrap().name, "8000_AccessLog.txt");
        logs.select(0);
        assert!(logs.parsing_status().is_some());
        logs.cancel();
        assert!(logs.parsing_status().is_none());
        logs.select(0);
        assert!(logs.parsing_status().is_some());
        let deadline = Instant::now() + Duration::from_secs(20);
        while !logs.parse_jobs.is_empty() && Instant::now() < deadline {
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(5));
        }
        logs.tick(&runtime, true);
        assert!(
            logs.parse_jobs.is_empty(),
            "background parser did not finish"
        );
        assert!(logs.visible_text().contains("recent"));
        assert!(!logs.visible_text().contains("outside"));
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert!(restored.snapshots[&source.key()].text.is_empty());
        assert_eq!(
            restored.snapshots[&source.key()].text_bytes,
            text.len() as u64
        );
        restored.cancel();
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn reset_command_resets_the_log_cursor_and_window() {
        let mut app = crate::tests::test_app();
        app.server_logs = viewer();
        let offset = FixedOffset::east_opt(0).unwrap();
        let earlier = presentation::parse_time("2026-10-14 08:00:00", offset);
        app.server_logs.apply_window(
            TimeWindow {
                start: earlier,
                end: earlier,
            },
            None,
        );
        app.server_logs.cursor_time = earlier;
        app.server_logs.scope_days = 7;
        app.server_logs.open_truncation_dialog();
        app.server_logs.truncation_dialog.as_mut().unwrap().fields[0] = "unfinished".into();
        app.server_logs.close_truncation_dialog();
        app.command_input = ":reset".into();
        app.execute_command();
        assert_eq!(app.mode, crate::AppMode::MarkLogicLogs);
        assert!(app.server_logs.cursor_time.is_none());
        let expected = app.server_logs.now() - Days::minutes(10);
        assert!(
            (app.server_logs.window.start.unwrap() - expected)
                .num_seconds()
                .abs()
                < 2
        );
        assert!(app.server_logs.window.end.is_none());
        assert_eq!(app.server_logs.scope_days, 1);
        assert!(app.server_logs.truncation_draft.is_none());
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn default_truncation_uses_server_time_minus_ten_minutes() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        logs.default_truncation = true;
        logs.window_initialized = true;
        logs.tx
            .send(CollectorEvent::Clock("2026-10-14T09:00:00+00:00".into()))
            .unwrap();
        logs.tick(&runtime, false);
        assert_eq!(
            logs.window.start,
            NaiveDate::from_ymd_opt(2026, 10, 14)
                .unwrap()
                .and_hms_opt(8, 50, 0)
        );
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
        logs.move_host_anchor_to_edge(false);
        logs.move_host_anchor(1);
        logs.set_truncation_at_cursor(true);
        assert_eq!(
            logs.window.start.unwrap().format("%H:%M").to_string(),
            "08:05"
        );
        assert!(!logs.visible_text().contains("outside-before"));
        logs.move_host_anchor(1);
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
        assert!(logs.matches.iter().all(|&i| logs.rows[i].truncation_marker));
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
        assert!(!merged.contains("first-inside") && merged.contains("other-inside"));
        assert!(!merged.contains("outside-before") && !merged.contains("other-after"));
        logs.search = "outside".into();
        logs.filter();
        assert!(logs.matches.iter().all(|&i| logs.rows[i].truncation_marker));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn page_up_and_shift_u_undo_use_distinct_keys() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        let text: String = (0..8)
            .map(|minute| format!("2026-10-14 08:{minute:02}:00.000 Info: entry-{minute}\n"))
            .collect();
        logs.apply(response("one", vec![chunk("one", 0, &text, true)]))
            .unwrap();
        logs.view_height = 2;
        logs.files_focus = false;
        let offset = FixedOffset::east_opt(0).unwrap();
        let time = |value| presentation::parse_time(value, offset);
        let window = TimeWindow {
            start: time("2026-10-14 08:01:00"),
            end: None,
        };
        logs.apply_window(window, None);
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
        };
        send(&mut app, KeyCode::Char('G'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, time("2026-10-14 08:05:00"));
        let cursor = app.server_logs.cursor_time;
        send(&mut app, KeyCode::Char('U'), KeyModifiers::NONE);
        assert_eq!(app.server_logs.window, TimeWindow::default());
        assert_eq!(app.server_logs.cursor_time, cursor);

        for (code, modifiers) in [
            (KeyCode::Char('U'), KeyModifiers::SHIFT),
            (KeyCode::Char('u'), KeyModifiers::SHIFT),
        ] {
            app.server_logs.apply_window(window, cursor);
            app.server_logs.files_focus = true;
            let mode = app.server_logs.mode_for_name("ErrorLog.txt");
            send(&mut app, code, modifiers);
            assert_eq!(app.server_logs.window, TimeWindow::default());
            assert_eq!(app.server_logs.cursor_time, cursor);
            assert_eq!(app.server_logs.mode_for_name("ErrorLog.txt"), mode);
        }
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_shortcuts_adjust_bounds_center_window_and_preserve_undo() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let runtime = Runtime::new().unwrap();
        let mut app = crate::tests::test_app();
        let mut logs = viewer();
        let text = "2026-10-14 09:00:00.000 Info: selected\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        let time =
            |value| presentation::parse_time(value, FixedOffset::east_opt(0).unwrap()).unwrap();
        logs.apply_window(
            TimeWindow {
                start: Some(time("2026-10-14 08:00:00")),
                end: Some(time("2026-10-14 10:00:00")),
            },
            None,
        );
        logs.files_focus = false;
        logs.view_height = 1;
        logs.move_host_anchor_to_edge(false);
        app.server_logs = logs;
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, c: char| {
            let modifiers = if c.is_ascii_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            crate::events::handle_key_event(app, KeyEvent::new(KeyCode::Char(c), modifiers))
                .unwrap();
        };
        let expire = |app: &mut crate::App| {
            app.server_logs
                .truncation_sequence
                .as_mut()
                .unwrap()
                .last_key = Instant::now() - TRUNCATION_KEY_TIMEOUT;
            app.server_logs.tick(&runtime, true);
        };
        for c in "5t".chars() {
            send(&mut app, c);
        }
        assert_eq!(
            app.server_logs.window.start,
            Some(time("2026-10-14 07:55:00"))
        );
        for c in "10t".chars() {
            send(&mut app, c);
        }
        assert_eq!(
            app.server_logs.window.start,
            Some(time("2026-10-14 07:45:00"))
        );
        for c in "t5".chars() {
            send(&mut app, c);
        }
        expire(&mut app);
        assert_eq!(
            app.server_logs.window.start,
            Some(time("2026-10-14 07:50:00"))
        );
        for c in "5T".chars() {
            send(&mut app, c);
        }
        assert_eq!(
            app.server_logs.window.end,
            Some(time("2026-10-14 09:55:00"))
        );
        for c in "T5".chars() {
            send(&mut app, c);
        }
        expire(&mut app);
        assert_eq!(
            app.server_logs.window.end,
            Some(time("2026-10-14 10:00:00"))
        );
        let previous = app.server_logs.window;
        for c in "tt".chars() {
            send(&mut app, c);
        }
        assert_eq!(
            app.server_logs.window,
            TimeWindow {
                start: Some(time("2026-10-14 08:57:30")),
                end: Some(time("2026-10-14 09:02:30")),
            }
        );
        app.server_logs.undo_truncation();
        assert_eq!(app.server_logs.window, previous);
        send(&mut app, 't');
        expire(&mut app);
        assert_eq!(
            app.server_logs.window.start,
            Some(time("2026-10-14 09:00:00"))
        );
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn truncation_shortcuts_handle_unset_bounds_and_reject_invalid_ranges() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        let mut logs = viewer();
        let text = "2026-10-14 09:00:00.000 Info: selected\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.files_focus = false;
        logs.move_host_anchor_to_edge(false);
        app.server_logs = logs;
        app.mode = crate::AppMode::MarkLogicLogs;
        for c in ['5', 't'] {
            crate::events::handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .unwrap();
        }
        let time = |value| presentation::parse_time(value, FixedOffset::east_opt(0).unwrap());
        assert_eq!(app.server_logs.window.start, time("2026-10-14 08:55:00"));
        for c in ['1', '0', 'T'] {
            crate::events::handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(app.server_logs.window.end, None);
        assert!(
            app.server_logs
                .message
                .contains("Start must not be after end")
        );
        let previous = app.server_logs.window;
        for c in "999999999999999999999999t".chars() {
            crate::events::handle_key_event(
                &mut app,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            )
            .unwrap();
        }
        assert_eq!(app.server_logs.window, previous);
        assert!(app.server_logs.message.contains("too large"));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
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
        let sequence = app.server_logs.truncation_sequence.take().unwrap();
        app.server_logs.finish_truncation_sequence(sequence);
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
        let sequence = app.server_logs.truncation_sequence.take().unwrap();
        app.server_logs.finish_truncation_sequence(sequence);
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
        logs.cursor_time = None;
        logs.move_host_anchor_to_edge(false);
        logs.move_host_anchor(1);
        logs.move_host_anchor(1);
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
        assert_eq!(other.rows.len(), 2);
        assert!(other.rows[0].truncation_marker);
        assert!(
            logs.display_host_row(other, 1)
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
        logs.move_host_anchor_to_edge(false);
        logs.move_host_anchor(1);
        let target = logs.anchor_timestamp().unwrap();
        let other = &logs.host_views[1];
        let nearest = logs.host_anchor(other).unwrap();
        assert_eq!(
            other.rows[other.matches[nearest]].timestamp.unwrap(),
            target + Days::seconds(10)
        );
        logs.cycle_leading_host(false);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        assert_eq!(logs.anchor_timestamp().unwrap(), target);
        assert_eq!(
            logs.current_line_time().unwrap(),
            target + Days::seconds(10)
        );
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
        for message in ["EE: two", "EE: beta"] {
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
        assert_eq!(logs.anchor_position(), Some(1));
        logs.cycle_focus(false);
        assert_eq!(logs.selected_file().unwrap().host, "two");
        assert_eq!(logs.anchor_position(), Some(1));
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
        assert_eq!(logs.anchor_position(), Some(2));
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
                    row: area.y + 2 + (row - offset) as u16,
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
    fn combined_view_merges_selected_port_across_hosts_within_truncation() {
        let mut logs = viewer();
        let sources = [
            (
                "one",
                "8000_ErrorLog.txt",
                "2026-10-13 23:58:00.000 Info: before-window\n2026-10-13 23:59:00.000 Info: error-one\n2026-10-14 00:03:00.000 Info: after-window\n",
            ),
            (
                "two",
                "8000_ErrorLog.txt",
                "2026-10-14 00:02:00.000 Error: error-two\n2026-10-14 00:02:00.000 Error+: stack-detail\n",
            ),
            (
                "one",
                "8000_ErrorLog_1.txt",
                "2026-10-13 23:59:30.000 Info: rotation-inside\n",
            ),
            (
                "one",
                "8000_AccessLog.txt",
                "127.0.0.1 - admin [14/Oct/2026:00:00:00 +0000] \"GET /access-one HTTP/1.1\" 200 42\n",
            ),
            (
                "two",
                "8000_AccessLog.txt",
                "127.0.0.1 - admin [14/Oct/2026:00:01:00 +0000] \"GET /access-two HTTP/1.1\" 200 42\n",
            ),
            (
                "one",
                "8000_RequestLog.txt",
                "{\"time\":\"2026-10-14T00:01:00Z\",\"message\":\"request-excluded\"}\n",
            ),
            (
                "one",
                "9000_ErrorLog.txt",
                "2026-10-14 00:01:00.000 Info: other-port-error\n",
            ),
            (
                "two",
                "9000_AccessLog.txt",
                "127.0.0.1 - admin [14/Oct/2026:00:02:00 +0000] \"GET /other-port-access HTTP/1.1\" 200 42\n",
            ),
        ];
        for (host, name, text) in sources {
            let file = named_file(host, name, "2026-10-14T01:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        let parse = |value| presentation::parse_time(value, FixedOffset::east_opt(0).unwrap());
        logs.apply_window(
            TimeWindow {
                start: parse("2026-10-13 23:59:00"),
                end: parse("2026-10-14 00:02:00"),
            },
            None,
        );
        let selected = logs
            .files
            .iter()
            .position(|file| file.name == "8000_ErrorLog.txt")
            .unwrap();
        logs.select(selected);
        logs.file_filter = "ErrorLog".into();
        logs.toggle_combined();
        let text = logs.visible_text();
        for entry in [
            "error-one",
            "rotation-inside",
            "access-one",
            "access-two",
            "error-two",
            "stack-detail",
        ] {
            assert!(text.contains(entry), "missing {entry}: {text}");
        }
        for entry in [
            "before-window",
            "after-window",
            "request-excluded",
            "other-port-error",
            "other-port-access",
        ] {
            assert!(!text.contains(entry), "unexpected {entry}: {text}");
        }
        assert!(text.find("error-one").unwrap() < text.find("rotation-inside").unwrap());
        assert!(text.find("access-one").unwrap() < text.find("access-two").unwrap());
        assert!(text.find("access-two").unwrap() < text.find("error-two").unwrap());

        logs.search = "stack-detail".into();
        logs.filter();
        let text = logs.visible_text();
        assert!(text.contains("error-two") && text.contains("stack-detail"));
        assert!(!text.contains("access-one"));

        logs.search.clear();
        let cursor = logs.cursor_time;
        let selected = logs
            .files
            .iter()
            .position(|file| file.name == "9000_ErrorLog.txt")
            .unwrap();
        logs.select(selected);
        let text = logs.visible_text();
        assert!(text.contains("other-port-error") && text.contains("other-port-access"));
        assert!(!text.contains("error-one") && !text.contains("access-one"));
        assert_eq!(logs.cursor_time, cursor);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn merged_rows_show_dates_and_sort_records_across_midnight() {
        let mut logs = viewer();
        let sources = [
            (
                "one",
                "9061_ErrorLog.txt",
                "2026-10-27 01:30:02.454 Info: next-day\n2026-10-26 19:00:00.000 Info: earlier\n",
            ),
            (
                "two",
                "9061_AccessLog.txt",
                "127.0.0.1 - u [26/Oct/2026:20:28:10 +0000] \"GET /previous-day HTTP/1.1\" 200 1\n",
            ),
            (
                "three",
                "9061_ErrorLog.txt",
                "2026-10-27 01:31:00.000 Info: later\n",
            ),
        ];
        for (host, name, text) in sources {
            let file = named_file(host, name, "2026-10-27T02:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select(0);
        let offset = FixedOffset::east_opt(0).unwrap();
        logs.apply_window(
            TimeWindow {
                start: presentation::parse_time("2026-10-26 20:00:00", offset),
                end: presentation::parse_time("2026-10-27 01:30:30", offset),
            },
            None,
        );
        logs.toggle_combined();
        for wrap in [false, true] {
            logs.options.wrap = wrap;
            logs.view_width = 120;
            logs.rebuild_lines();
            let text = logs.visible_text();
            let previous = text
                .lines()
                .find(|line| line.contains("previous-day"))
                .unwrap();
            let next = text.lines().find(|line| line.contains("next-day")).unwrap();
            assert!(previous.contains("2026-10-26 20:28:10.000"), "{previous}");
            assert!(next.contains("2026-10-27 01:30:02.454"), "{next}");
            assert!(text.find(previous).unwrap() < text.find(next).unwrap());
            assert!(!text.contains("earlier") && !text.contains("later"));
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn merged_order_survives_late_out_of_order_records_and_unusual_log_lines() {
        let mut logs = viewer();
        let sources = [
            (
                "one",
                "9061_ErrorLog.txt",
                "2026-10-28 08:00:00.000 Info: error-one\n",
            ),
            (
                "two",
                "9061_ErrorLog.txt",
                "2026-10-28 08:06:00.000 Info: error-two\n",
            ),
            (
                "three",
                "9061_ErrorLog.txt",
                "2026-10-28 08:08:00.000 Info: error-three\n",
            ),
            (
                "one",
                "9061_AccessLog.txt",
                "127.0.0.1 - u [28/Oct/2026:08:03:00 +0000] \"GET /access-one HTTP/1.1\" 200 1\n",
            ),
            (
                "two",
                "9061_AccessLog.txt",
                "127.0.0.1 - u [28/Oct/2026:08:07:00 +0000] \"GET /access-two HTTP/1.1\" 200 1\n",
            ),
            (
                "three",
                "9061_AccessLog.txt",
                "127.0.0.1 - u [28/Oct/2026:08:09:00 +0000] \"GET /access-three HTTP/1.1\" 200 1\n",
            ),
        ];
        for (host, name, text) in sources {
            let file = named_file(host, name, "2026-10-28T09:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select(0);
        let offset = FixedOffset::east_opt(0).unwrap();
        logs.apply_window(
            TimeWindow {
                start: presentation::parse_time("2026-10-28 08:00:00", offset),
                end: presentation::parse_time("2026-10-28 08:10:00", offset),
            },
            None,
        );
        logs.toggle_combined();
        let rt = Runtime::new().unwrap();
        let late_updates = [
            (
                "two",
                "9061_AccessLog.txt",
                "127.0.0.1 - u [28/Oct/2026:08:02:00.500 +0000] unconventional-access\nundated-access\n",
            ),
            (
                "three",
                "9061_ErrorLog.txt",
                "2026-10-28 08:01:30.000 - unconventional-error\n",
            ),
        ];
        for (host, name, text) in late_updates {
            let file = logs
                .files
                .iter()
                .find(|file| file.host == host && file.name == name)
                .unwrap()
                .clone();
            let key = file.key();
            let old_len = logs.commit(file, false, text, |_| {}).unwrap();
            logs.extend_view(&key, false, old_len);
            logs.tick(&rt, false);
            let entries: Vec<_> = logs
                .matches
                .iter()
                .filter_map(|&index| {
                    let row = logs.rows.get(index)?;
                    (row.start && row.timestamp.is_some())
                        .then(|| (row.timestamp.unwrap(), logs.display_row(index).to_string()))
                })
                .collect();
            assert!(entries.windows(2).all(|pair| pair[0].0 <= pair[1].0));
            for (timestamp, line) in &entries {
                assert!(
                    line.contains(&timestamp.format("%Y-%m-%d %H:%M:%S%.3f").to_string()),
                    "{line}"
                );
            }
        }
        let text = logs.visible_text();
        assert!(text.contains("unconventional-access") && text.contains("unconventional-error"));
        assert!(text.find("error-one").unwrap() < text.find("unconventional-error").unwrap());
        assert!(
            text.find("unconventional-error").unwrap()
                < text.find("unconventional-access").unwrap()
        );
        assert!(text.find("unconventional-access").unwrap() < text.find("access-one").unwrap());
        assert!(!text.contains("undated-access"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn merge_key_shows_one_purple_frame_with_six_files_and_keeps_truncation() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let mut logs = viewer();
        let sources = [
            (
                "one",
                "8000_ErrorLog.txt",
                "2026-10-14 08:00:00.000 Info: before-window\n2026-10-14 08:05:00.000 Info: error-one\n2026-10-14 08:10:00.000 Info: end-entry\n2026-10-14 08:15:00.000 Info: after-window\n",
            ),
            (
                "two",
                "8000_ErrorLog.txt",
                "2026-10-14 08:06:00.000 Info: error-two\n",
            ),
            (
                "three",
                "8000_ErrorLog.txt",
                "2026-10-14 08:08:00.000 Info: error-three\n",
            ),
            (
                "one",
                "8000_AccessLog.txt",
                "127.0.0.1 - u [14/Oct/2026:08:09:00 +0000] \"GET /access-one HTTP/1.1\" 200 1\n",
            ),
            (
                "two",
                "8000_AccessLog.txt",
                "127.0.0.1 - u [14/Oct/2026:08:07:00 +0000] \"GET /access-two HTTP/1.1\" 200 1\n",
            ),
            (
                "three",
                "8000_AccessLog.txt",
                "127.0.0.1 - u [14/Oct/2026:08:05:20 +0000] \"GET /access-three HTTP/1.1\" 200 1\n",
            ),
        ];
        for (host, name, text) in sources {
            let file = named_file(host, name, "2026-10-14T09:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select(0);
        logs.files_focus = false;
        let mut app = crate::tests::test_app();
        app.server_logs = logs;
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, ch| {
            crate::events::handle_key_event(
                app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            )
            .unwrap();
        };
        for ch in ['g', 'j', 't', 'j', 'T'] {
            send(&mut app, ch);
        }
        let sequence = app.server_logs.truncation_sequence.take().unwrap();
        app.server_logs.finish_truncation_sequence(sequence);
        let window = app.server_logs.window;
        let offset = FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            window.start,
            presentation::parse_time("2026-10-14 08:05:00", offset)
        );
        assert_eq!(
            window.end,
            presentation::parse_time("2026-10-14 08:10:00", offset)
        );
        let cursor = app.server_logs.cursor_time;
        send(&mut app, 'm');
        assert!(app.server_logs.combined);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        assert!(app.server_logs.host_views.is_empty());

        let text = app.server_logs.visible_text();
        let entries = [
            "error-one",
            "access-three",
            "error-two",
            "access-two",
            "error-three",
            "access-one",
            "end-entry",
        ];
        let mut previous = 0;
        for entry in entries {
            let position = text
                .find(entry)
                .unwrap_or_else(|| panic!("missing {entry}: {text}"));
            assert!(
                position >= previous,
                "merged entries are out of order: {text}"
            );
            previous = position;
        }
        assert!(!text.contains("before-window") && !text.contains("after-window"));
        for (entry, host) in [
            ("error-one", "one"),
            ("access-three", "three"),
            ("error-two", "two"),
            ("access-two", "two"),
            ("error-three", "three"),
            ("access-one", "one"),
        ] {
            let line = text.lines().find(|line| line.contains(entry)).unwrap();
            assert!(
                line.contains(&format!(" {} ", app.server_logs.host_alias(host))),
                "{line}"
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(180, 35)).unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        let content_areas: Vec<_> = app
            .server_logs
            .mouse_regions
            .iter()
            .filter_map(|(area, target)| matches!(target, LogMouseTarget::Content).then_some(*area))
            .collect();
        assert_eq!(content_areas.len(), 1);
        assert!(
            !app.server_logs
                .mouse_regions
                .iter()
                .any(|(_, target)| matches!(target, LogMouseTarget::Host(_)))
        );
        let area = content_areas[0];
        assert_eq!(
            terminal.backend().buffer()[(area.x, area.y)].fg,
            Color::Magenta
        );
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("MERGED port 8000"));
        assert!(screen.contains("m: unmerge"));
        assert!(screen.contains("HOST"));
        let files_area = app
            .server_logs
            .mouse_regions
            .iter()
            .find_map(|(area, target)| {
                matches!(target, LogMouseTarget::Files { .. }).then_some(*area)
            })
            .unwrap();
        let header: String = (files_area.x..files_area.x + files_area.width)
            .map(|x| terminal.backend().buffer()[(x, files_area.y + 1)].symbol())
            .collect();
        assert!(header.contains("Mrg") && header.contains("Req"), "{header}");
        assert!(
            !header.contains("Err") && !header.contains("Acc"),
            "{header}"
        );
        for entry in entries {
            assert!(screen.contains(entry), "merged frame does not show {entry}");
        }
        assert!(!screen.contains("before-window") && !screen.contains("after-window"));
        assert!(screen.contains("log entries truncated"));

        app.server_logs.options.raw = true;
        app.server_logs.rebuild_lines();
        let raw = app.server_logs.visible_text();
        for (entry, host) in [
            ("error-one", "one"),
            ("access-three", "three"),
            ("access-two", "two"),
        ] {
            let line = raw.lines().find(|line| line.contains(entry)).unwrap();
            assert!(
                line.starts_with(&format!("{:<3} ", app.server_logs.host_alias(host))),
                "{line}"
            );
        }
        app.server_logs.options.raw = false;
        app.server_logs.rebuild_lines();

        crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(area.x, area.y)].fg,
            Color::Magenta
        );
        send(&mut app, 'm');
        assert!(!app.server_logs.combined);
        assert_eq!(app.server_logs.window, window);
        assert_eq!(app.server_logs.cursor_time, cursor);
        terminal
            .draw(|frame| crate::ui::ui(frame, &mut app))
            .unwrap();
        assert_eq!(
            app.server_logs
                .mouse_regions
                .iter()
                .filter(|(_, target)| matches!(target, LogMouseTarget::Host(_)))
                .count(),
            3
        );
        let text = app.server_logs.visible_text();
        assert!(text.contains("error-one") && text.contains("end-entry"));
        assert!(!text.contains("before-window") && !text.contains("after-window"));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
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
    fn clearing_cache_keeps_history_scope_and_subscription_settings() {
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
        assert_eq!(logs.scope_days, 7);
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
        assert_eq!(logs.scroll, 0, "new matches preserve the review cursor");
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
    fn download_percentage_weights_active_jobs_and_completes_failed_checks() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        logs.begin_activity("Refreshing logs", 2);
        let activity = logs.activity.as_mut().unwrap();
        activity.download = true;
        let small = Arc::new(AtomicU64::new(100));
        let large = Arc::new(AtomicU64::new(300));
        for (name, size, received, response_size) in [
            ("SmallLog.txt", 100, small, 200),
            ("LargeLog.txt", 300, large.clone(), 600),
        ] {
            activity.jobs.insert(
                ("one".into(), name.into()),
                DownloadProgress {
                    size: Some(size),
                    received,
                    response_size: Arc::new(AtomicU64::new(response_size)),
                    done: false,
                },
            );
        }
        assert!(logs.activity_status().unwrap().contains("~50% processed"));
        logs.tx
            .send(CollectorEvent::DownloadReport {
                host: "one".into(),
                file: "SmallLog.txt".into(),
                result: Err("timeout".into()),
                bytes: 100,
            })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(logs.activity_status().unwrap().contains("~62% processed"));
        assert!(logs.activity_status().unwrap().contains("1 failed"));
        large.store(600, Ordering::Relaxed);
        assert!(logs.activity_status().unwrap().contains("~99% processed"));
        logs.tx
            .send(CollectorEvent::DownloadReport {
                host: "one".into(),
                file: "LargeLog.txt".into(),
                result: Ok(response("one", vec![])),
                bytes: 600,
            })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(logs.activity_status().unwrap().contains("~100% processed"));
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
    fn files_keys_distinguish_tracked_columns_all_columns_and_days() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.server_logs = viewer();
        for name in [
            "8000_ErrorLog.txt",
            "8000_AccessLog.txt",
            "8000_RequestLog.txt",
            "8000_ErrorLog_1.txt",
        ] {
            app.server_logs
                .files
                .push(named_file("one", name, "2026-10-14T09:00:00Z"));
        }
        app.server_logs
            .modes
            .insert("8000_RequestLog.txt".into(), TrackingMode::OnDemand);
        app.server_logs.select(0);
        app.server_logs.files_focus = true;
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap()
        };
        send(&mut app, KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_RequestLog.txt"
        );
        send(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_AccessLog.txt"
        );
        send(&mut app, KeyCode::Char('H'), KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_AccessLog.txt"
        );
        send(&mut app, KeyCode::Char('h'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog.txt"
        );
        send(&mut app, KeyCode::Char('h'), KeyModifiers::CONTROL);
        assert_eq!(app.server_logs.discovery_days_ago, 1);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog_1.txt"
        );
        send(&mut app, KeyCode::Char('l'), KeyModifiers::CONTROL);
        assert_eq!(app.server_logs.discovery_days_ago, 0);
        send(&mut app, KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(app.server_logs.discovery_days_ago, 1);
        send(&mut app, KeyCode::Right, KeyModifiers::CONTROL);
        assert_eq!(app.server_logs.discovery_days_ago, 0);
        send(&mut app, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(app.server_logs.discovery_days_ago, 1);
        send(&mut app, KeyCode::Char('r'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.discovery_days_ago, 1,
            "enabling day shortcuts must preserve the selected day"
        );
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn file_column_navigation_finds_nearby_ports_when_current_cell_is_missing() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.server_logs = viewer();
        for name in [
            "8000_ErrorLog.txt",
            "8001_AccessLog.txt",
            "8002_RequestLog.txt",
        ] {
            app.server_logs
                .files
                .push(named_file("one", name, "2026-10-14T09:00:00Z"));
        }
        app.server_logs
            .modes
            .insert("8002_RequestLog.txt".into(), TrackingMode::OnDemand);
        app.server_logs.select(0);
        app.server_logs.files_focus = true;
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, key| {
            crate::events::handle_key_event(app, key).unwrap();
        };
        send(
            &mut app,
            KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
        );
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8002_RequestLog.txt"
        );
        send(&mut app, KeyEvent::new(KeyCode::Left, KeyModifiers::SHIFT));
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8001_AccessLog.txt"
        );
        send(
            &mut app,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog.txt"
        );
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn day_navigation_restores_port_and_column_when_inventory_arrives() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        logs.files = vec![
            named_file("one", "8000_AccessLog.txt", "2026-10-14T09:00:00Z"),
            named_file("one", "8001_ErrorLog_1.txt", "2026-10-13T09:00:00Z"),
        ];
        logs.select(0);
        logs.set_discovery_days_ago(1, &runtime);
        assert_eq!(logs.discovery_days_ago, 1);
        assert_eq!(logs.selected_file().unwrap().name, "8001_ErrorLog_1.txt");
        let mut inventory = response("one", vec![]);
        inventory.files = vec![named_file(
            "one",
            "8000_AccessLog_1.txt",
            "2026-10-13T09:00:00Z",
        )];
        logs.apply(inventory).unwrap();
        assert_eq!(logs.selected_file().unwrap().name, "8000_AccessLog_1.txt");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn merged_files_navigation_treats_error_and_access_as_one_column() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.server_logs = viewer();
        for name in [
            "8000_ErrorLog.txt",
            "8000_AccessLog.txt",
            "8000_RequestLog.txt",
            "8001_ErrorLog.txt",
            "8001_AccessLog.txt",
            "8001_RequestLog.txt",
        ] {
            app.server_logs
                .files
                .push(named_file("one", name, "2026-10-14T09:00:00Z"));
        }
        app.server_logs
            .modes
            .insert("8001_AccessLog.txt".into(), TrackingMode::Live);
        app.server_logs.select(2);
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap()
        };
        send(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(app.server_logs.selected_port(), Some("8000"));
        assert_eq!(app.server_logs.selected_display_column(), 0);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8000_ErrorLog.txt"
        );
        app.server_logs.files_focus = true;
        send(&mut app, KeyCode::Char('j'), KeyModifiers::NONE);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8001_AccessLog.txt"
        );
        assert_eq!(app.server_logs.selected_display_column(), 0);
        send(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8001_RequestLog.txt"
        );
        assert_eq!(app.server_logs.selected_display_column(), 1);
        send(&mut app, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(app.server_logs.selected_display_column(), 0);
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn merged_download_requests_both_files_on_all_hosts_and_queues_while_busy() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        let listener = app
            .rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = app.rt.spawn(async move {
            let mut checked = Vec::new();
            for _ in 0..12 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    headers.push(stream.read_u8().await.unwrap());
                }
                let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                let host = vars["expected"].as_str().unwrap();
                let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0]["full"], true);
                let path = requests[0]["path"].as_str().unwrap();
                let name = path.rsplit('/').next().unwrap();
                checked.push((host.to_owned(), name.to_owned()));
                let (status, response) = if host == "two" && name == "9061_AccessLog.txt" {
                    ("500 Internal Server Error", "read failed".to_string())
                } else {
                    ("200 OK", json!({"host":host,"directory":"/Logs","inventory":false,"read_bytes":3,"cluster_bytes":0,
                        "files":[{"host":host,"path":path,"name":name,"size":3,"modified":"2026-10-14T09:00:00Z"}],
                        "chunks":[{"host":host,"path":path,"start":0,"end":3,"length":3,"reset":true,"text":"abc","full":true}]}).to_string())
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
            checked
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        for host in ["one", "two", "three"] {
            for name in ["9061_ErrorLog.txt", "9061_AccessLog.txt"] {
                logs.files
                    .push(named_file(host, name, "2026-10-14T09:00:00Z"));
            }
        }
        for name in [
            "9061_RequestLog.txt",
            "9061_ErrorLog_1.txt",
            "9000_ErrorLog.txt",
        ] {
            logs.files
                .push(named_file("one", name, "2026-10-14T09:00:00Z"));
        }
        logs.endpoints = ["one", "two", "three"]
            .into_iter()
            .map(|host| Endpoint {
                host: host.into(),
                client: client.clone(),
            })
            .collect();
        logs.select(0);
        app.server_logs = logs;
        app.mode = crate::AppMode::MarkLogicLogs;
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap()
        };
        send(&mut app, KeyCode::Char('m'), KeyModifiers::NONE);
        send(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
        assert_eq!(app.server_logs.activity.as_ref().unwrap().total, 6);
        app.server_logs.files_focus = true;
        send(&mut app, KeyCode::Char('d'), KeyModifiers::NONE);
        assert!(
            app.server_logs
                .merged_downloads_queued
                .contains(&("9061".into(), 0))
        );
        let other = app
            .server_logs
            .files
            .iter()
            .position(|file| file.name == "9000_ErrorLog.txt")
            .unwrap();
        app.server_logs.select(other);
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                while app.server_logs.busy || !app.server_logs.merged_downloads_queued.is_empty() {
                    app.server_logs.tick(&app.rt, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        let checked = app.rt.block_on(server).unwrap();
        for host in ["one", "two", "three"] {
            for name in ["9061_ErrorLog.txt", "9061_AccessLog.txt"] {
                assert_eq!(
                    checked
                        .iter()
                        .filter(|(h, n)| h == host && n == name)
                        .count(),
                    2
                );
            }
        }
        assert_eq!(checked.len(), 12);
        assert_eq!(
            app.server_logs
                .download_results
                .get(&("two".into(), "9061_AccessLog.txt".into())),
            Some(&false)
        );
        assert!(app.server_logs.errors["two"].contains("9061_AccessLog.txt"));
        assert_eq!(
            app.server_logs.mode_for_name("9061_AccessLog.txt"),
            TrackingMode::Untracked
        );
        assert_eq!(
            app.server_logs.mode_for_name("9061_ErrorLog.txt"),
            TrackingMode::Untracked
        );
        let selected = app
            .server_logs
            .files
            .iter()
            .position(|file| file.name == "9061_ErrorLog.txt")
            .unwrap();
        app.server_logs.select(selected);
        let details = app.server_logs.merged_host_details();
        let (_, _, error, access, error_result, access_result) =
            details.iter().find(|(_, host, ..)| *host == "two").unwrap();
        assert!(error.is_some() && access.is_some());
        assert_eq!(*error_result, Some(true));
        assert_eq!(*access_result, Some(false));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
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
    #[test]
    fn content_filters_apply_to_split_cached_and_merged_views() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        let text = "2026-10-14 08:00:00.000 Info: alpha keep\n continuation matching\n2026-10-14 08:01:00.000 Info: alpha blocked\n2026-10-14 08:02:00.000 Info: beta keep\n";
        logs.apply(response("one", vec![chunk("one", 0, text, true)]))
            .unwrap();
        logs.files_focus = false;
        let set = |logs: &mut LogViewer, command, value: &str| {
            assert!(
                logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            );
            assert!(
                logs.handle_filter_prefix(KeyEvent::new(
                    KeyCode::Char(command),
                    KeyModifiers::NONE
                ))
            );
            for c in value.chars() {
                logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        };
        set(&mut logs, 'i', "alpha");
        set(&mut logs, 'o', "blocked");
        logs.context_before = 2;
        logs.context_after = 2;
        logs.search = "keep".into();
        logs.filter();
        let visible = logs.visible_text();
        assert!(visible.contains("alpha keep"));
        assert!(!visible.contains("blocked"));
        assert!(!visible.contains("beta keep"));
        logs.search.clear();
        logs.toggle_all_cached();
        let visible = logs.visible_text();
        assert!(visible.contains("alpha keep"));
        assert!(visible.contains("continuation matching"));
        assert!(!visible.contains("blocked"));
        assert!(!visible.contains("beta keep"));
        logs.toggle_all_cached();
        logs.toggle_combined();
        let visible = logs.visible_text();
        assert!(visible.contains("alpha keep"));
        assert!(!visible.contains("blocked"));
        assert!(!visible.contains("beta keep"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn failed_replacement_keeps_published_generation_readable() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "old\n", true)]))
            .unwrap();
        let original_path = logs.local_path().unwrap();
        let stem = hash_key(&file("one").key());
        let blocker = logs.cache_root.join(format!("{stem}.tmp"));
        fs::create_dir(&blocker).unwrap();
        assert!(
            logs.apply(response("one", vec![chunk("one", 0, "new\n", true)]))
                .is_err()
        );
        assert_eq!(fs::read_to_string(&original_path).unwrap(), "old\n");
        fs::remove_dir(blocker).unwrap();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(
            fs::read_to_string(restored.local_path().unwrap()).unwrap(),
            "old\n"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn next_append_discards_an_uncommitted_suffix_after_restart() {
        let mut logs = viewer();
        logs.apply(response("one", vec![chunk("one", 0, "old\n", true)]))
            .unwrap();
        let path = logs.local_path().unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"orphan\n")
            .unwrap();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        restored
            .apply(response("one", vec![chunk("one", 4, "new\n", false)]))
            .unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "old\nnew\n");
        let mut again = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        again.load_cache().unwrap();
        assert_eq!(again.snapshots[&file("one").key()].text, "old\nnew\n");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn collector_events_are_processed_in_bounded_batches() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        for _ in 0..20 {
            logs.tx
                .send(CollectorEvent::Clock("invalid".into()))
                .unwrap();
        }
        logs.tick(&runtime, false);
        assert_eq!(logs.rx.len(), 4);
        logs.tick(&runtime, false);
        assert_eq!(logs.rx.len(), 0);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn editor_export_respects_content_filters_without_search_or_window() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        let raw = "2026-10-14 08:00:00.000 Info: keep\n2026-10-14 08:01:00.000 Info: omit\n";
        logs.apply(response("one", vec![chunk("one", 0, raw, true)]))
            .unwrap();
        logs.files_focus = false;
        for c in ['f', 'i'] {
            assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
        }
        for c in "keep".chars() {
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let path = logs.editor_export().unwrap();
        let exported = fs::read_to_string(&path).unwrap();
        assert!(exported.contains("keep"));
        assert!(!exported.contains("omit"));
        let _ = fs::remove_file(path);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn forced_download_is_coalesced_and_remembers_the_requested_file() {
        let mut logs = viewer();
        logs.files = vec![
            file("one"),
            named_file("one", "8040_ErrorLog.txt", "2026-10-14T12:00:00Z"),
        ];
        let runtime = Runtime::new().unwrap();
        logs.busy = true;
        logs.download_full(&runtime);
        logs.download_full(&runtime);
        assert_eq!(
            logs.forced_downloads_queued
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["ErrorLog.txt"]
        );
        logs.selected = 1;
        logs.download_full(&runtime);
        assert_eq!(logs.forced_downloads_queued.len(), 2);
        assert_eq!(
            logs.mode_for_name("8040_ErrorLog.txt"),
            TrackingMode::OnDemand
        );
        logs.set_selected_mode(logs.files[1].clone(), TrackingMode::Untracked);
        assert!(!logs.forced_downloads_queued.contains("8040_ErrorLog.txt"));
        logs.cancel();
        assert!(logs.forced_downloads_queued.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn queued_forced_download_starts_after_current_job() {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") { headers.push(stream.read_u8().await.unwrap()); }
            let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
            let mut body = vec![0; length]; stream.read_exact(&mut body).await.unwrap();
            let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
            let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
            let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
            let path = requests[0]["path"].as_str().unwrap();
            assert_eq!(path, "/Logs/ErrorLog.txt");
            assert_eq!(requests[0]["full"], true);
            let response = json!({"host":"one","directory":"/Logs","inventory":false,"read_bytes":3,"cluster_bytes":0,
                "files":[{"host":"one","path":path,"name":"ErrorLog.txt","size":3,"modified":"2026-10-14T09:00:00Z"}],
                "chunks":[{"host":"one","path":path,"start":0,"end":3,"length":3,"reset":true,"text":"abc","full":true}]}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        logs.files = vec![
            file("one"),
            named_file("one", "8000_ErrorLog.txt", "2026-10-14T09:00:00Z"),
        ];
        logs.endpoints = vec![Endpoint {
            host: "one".into(),
            client,
        }];
        logs.busy = true;
        logs.download_full(&runtime);
        logs.selected = 1;
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&runtime, false);
        assert!(logs.busy);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while logs.busy {
                    logs.tick(&runtime, false);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                server.await.unwrap();
            })
            .await
            .unwrap();
        });
        assert_eq!(logs.snapshots[&file("one").key()].text, "abc");
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
}
