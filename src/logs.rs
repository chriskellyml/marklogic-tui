//! Host-scoped query collectors and a persistent, locally searchable log cache.
use crate::client::MarkLogicClient;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration as Days, NaiveDate, NaiveDateTime, Timelike, Utc};
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
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{runtime::Runtime, sync::mpsc, task::AbortHandle, task::JoinSet};
mod activity;
mod archive;
mod cache_load;
mod downloads;
mod filter;
mod history;
mod index;
pub(crate) mod presentation;
mod service_health;
pub(crate) mod storage;
mod ui_updates;
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
    Hosts,
    HostChoice(usize),
    Content,
    Activity,
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
    pub keys: Vec<String>,
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
    staging: BTreeSet<(String, String)>,
    last: Option<(String, String, bool)>,
    jobs: BTreeMap<(String, String), DownloadProgress>,
    work_bytes: Option<u64>,
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
const PREVIEW_DELAY: Duration = Duration::from_millis(300);
const STATUS_INTERVAL: Duration = Duration::from_secs(60);
const READ_RETRY_INTERVAL: Duration = Duration::from_secs(60);
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
    /// Filename within the archive directory for this committed generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    storage: Option<String>,
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
    fn source_identity(&self) -> String {
        self.archive
            .as_deref()
            .map(history::archive_identity)
            .unwrap_or_else(|| self.file.key())
    }
    fn index_path(&self, root: &Path) -> PathBuf {
        self.archive_path(root)
            .map(|path| path.with_extension("records.sqlite"))
            .unwrap_or_else(|| root.join(format!("{}.records.sqlite", hash_key(&self.file.key()))))
    }
    fn content_path(&self, root: &Path) -> PathBuf {
        let stem = hash_key(&self.file.key());
        if self.generation_path {
            root.join(format!("{stem}.{}.generation.log", self.generation))
        } else {
            root.join(format!("{stem}.log"))
        }
    }
    fn archive_path(&self, root: &Path) -> Option<PathBuf> {
        let name = self.archive.as_deref()?;
        let path = Path::new(name);
        if path.file_name()?.to_str()? != name || path.extension()?.to_str()? != "gz" {
            return None;
        }
        Some(root.join("archives").join(path))
    }
    fn storage_path(&self, root: &Path) -> Option<PathBuf> {
        let name = self.storage.as_deref()?;
        if Path::new(name).file_name()?.to_str()? != name || !name.ends_with(".parts") {
            return None;
        }
        Some(root.join("storage").join(name))
    }
    fn read_path(&self, root: &Path) -> PathBuf {
        self.storage_path(root)
            .or_else(|| self.archive_path(root))
            .unwrap_or_else(|| self.content_path(root))
    }
    fn open_content(&self, root: &Path) -> std::io::Result<Box<dyn index::ReadSeek>> {
        if self.storage.is_some() {
            let path = self
                .storage_path(root)
                .ok_or_else(|| std::io::Error::other("Invalid daily storage path"))?;
            let reader = storage::PartsReader::open(&path)?;
            if reader.len() != self.text_bytes {
                return Err(std::io::Error::other("Daily storage length mismatch"));
            }
            return Ok(Box::new(reader));
        }
        if self.archive.is_some() {
            let archived = self
                .archive_path(root)
                .ok_or_else(|| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid archive path")
                })
                .and_then(|path| archive::ArchiveReader::open(&path))
                .and_then(|reader| {
                    if reader.raw_len() == self.text_bytes {
                        Ok(reader)
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Archive size does not match its committed generation",
                        ))
                    }
                });
            match archived {
                Ok(reader) => return Ok(Box::new(reader)),
                Err(error) => {
                    if let Ok(file) = fs::File::open(self.content_path(root))
                        && file
                            .metadata()
                            .is_ok_and(|metadata| metadata.len() >= self.text_bytes)
                    {
                        return Ok(Box::new(file));
                    }
                    return Err(error);
                }
            }
        } else {
            let file = fs::File::open(self.content_path(root))?;
            if file.metadata()?.len() < self.text_bytes {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Cache is shorter than its committed size",
                ));
            }
            Ok(Box::new(file))
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
            archive: self.archive.clone(),
            storage: self.storage.clone(),
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
    #[serde(skip)]
    staged: Option<StagedChunk>,
}

/// A completed large response waiting to be published as a cache generation.
/// Dropping an undelivered response, including after cancellation, removes it.
#[derive(Debug)]
struct StagedChunk {
    path: PathBuf,
    bytes: u64,
    chars: u64,
}

impl Drop for StagedChunk {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct StageCancellation(Arc<AtomicBool>);

impl Drop for StageCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
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
    before: Option<i64>,
    paged: bool,
    filter_id: u64,
}

impl ParseSpec {
    fn serves_prefix_of(self, next: Self) -> bool {
        self.bytes <= next.bytes
            && Self {
                bytes: next.bytes,
                ..self
            } == next
    }
}

struct ReadFailure {
    spec: ParseSpec,
    at: Instant,
}

struct PageNavigation {
    key: String,
    generation: u64,
    window: TimeWindow,
    cursors: Vec<Option<i64>>,
    pinned_bytes: u64,
    pending: Option<(Vec<Option<i64>>, bool)>,
}

struct ParseJob {
    spec: ParseSpec,
    progress: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    phase: Arc<AtomicU8>,
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
    fallback_reason: Option<String>,
    result: std::io::Result<(Stream, bool)>,
}

struct ArchiveJob {
    key: String,
    name: String,
    cancelled: Arc<AtomicBool>,
}

struct ArchivedRawCleanup {
    key: String,
    generation: u64,
    bytes: u64,
    path: PathBuf,
    retry_at: Instant,
}

impl Drop for ArchiveJob {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct ArchiveArtifact {
    path: PathBuf,
    keep: bool,
}

impl Drop for ArchiveArtifact {
    fn drop(&mut self) {
        if !self.keep {
            archive::remove(&self.path);
        }
    }
}

struct ArchiveResult {
    key: String,
    generation: u64,
    bytes: u64,
    name: String,
    cancelled: Arc<AtomicBool>,
    metadata: Vec<u8>,
    result: Result<ArchiveArtifact, String>,
}

// The OS releases this lock even if a storage worker or the process exits.
// Keep the inode in place so other processes always lock the same object.
struct ArchivePublishLock {
    _file: fs::File,
}
impl ArchivePublishLock {
    fn acquire(root: &Path) -> Result<Self> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".storage.lock"))?;
        file.try_lock()
            .context("Another storage operation is using this cache")?;
        Ok(Self { _file: file })
    }
}

enum CollectorEvent {
    Endpoints(Vec<Endpoint>),
    DownloadStarted {
        host: String,
        file: String,
    },
    DownloadStaging {
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
    HistoryPromoted {
        host: String,
        file: String,
        result: Result<Box<Snapshot>, String>,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TrackingPeriod {
    start: NaiveDate,
    end: Option<NaiveDate>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CacheFreshness {
    Fresh,
    Stale,
    Missing,
    NoData,
}

impl TrackingMode {
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
    include_untimed: bool,
    kinds: [bool; 4],
}

impl MergeSource<'_> {
    fn includes(&self, index: usize) -> bool {
        let record = &self.stream.records[index];
        (self.window.contains(record.timestamp)
            || (self.include_untimed && record.timestamp.is_none()))
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
    fn new(
        streams: &[&'a Stream],
        window: TimeWindow,
        include_untimed: bool,
        kinds: [bool; 4],
    ) -> Self {
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
                include_untimed,
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
    search_changed_at: Option<Instant>,
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
    /// Logical filenames selected in the grid, including every host copy.
    selected_log_names: BTreeSet<String>,
    /// Explicit row selections span every available day.
    selection_across_days: bool,
    rendered_day: Option<NaiveDate>,
    preview_changed: Option<Instant>,
    sidebar_status: RefCell<Option<Arc<ui_updates::SidebarStatus>>>,
    sidebar_rows: RefCell<ui_updates::SidebarRows>,
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
    /// The initial ten-minute window applies to today, not historical dates.
    automatic_window: bool,
    /// The session window has been initialized, including an explicitly cleared window.
    window_initialized: bool,
    pub truncation_dialog: Option<TruncationDialog>,
    truncation_draft: Option<TruncationDialog>,
    truncation_sequence: Option<TruncationSequence>,
    truncation_dialog_opened_at: Option<Instant>,
    pub entry_detail: Option<LogEntryDetail>,
    pub view_width: usize,
    pub rows: Vec<ViewRow>,
    pub host_views: Vec<HostView>,
    pub activity_graph: activity::ActivityGraph,
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
    pub hosts_focus: bool,
    pub host_selected: usize,
    excluded_hosts: BTreeSet<String>,
    pub host_pane_maximized: bool,
    pub show_line_numbers: bool,
    parsed_streams: HashMap<String, Stream>,
    /// Dates in retained historical archive generations, keyed by synthetic source.
    retained_archive_days: HashMap<String, BTreeSet<NaiveDate>>,
    parsed_large: HashMap<String, (ParseSpec, bool)>,
    page_navigation: Option<PageNavigation>,
    index_filter_active: Option<String>,
    index_filter_target: Option<String>,
    index_filter_changed: Option<Instant>,
    index_filter_revision: u64,
    parse_jobs: HashMap<String, ParseJob>,
    read_failures: HashMap<String, ReadFailure>,
    parse_lock: Arc<std::sync::Mutex<()>>,
    parse_tx: mpsc::UnboundedSender<ParseResult>,
    parse_rx: mpsc::UnboundedReceiver<ParseResult>,
    archive_job: Option<ArchiveJob>,
    archived_raw_cleanup: Vec<ArchivedRawCleanup>,
    archive_tx: mpsc::UnboundedSender<ArchiveResult>,
    archive_rx: mpsc::UnboundedReceiver<ArchiveResult>,
    archive_retry: HashMap<String, Instant>,
    archive_last_scan: Option<Instant>,
    archive_scan_day: Option<NaiveDate>,
    archive_dirty: bool,
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
    tracking_periods: BTreeMap<String, Vec<TrackingPeriod>>,
    /// Explicit per-day choices for rotated logs, stable as suffixes change.
    historical_tracking: BTreeMap<String, BTreeMap<NaiveDate, bool>>,
    history_checked: Option<NaiveDate>,
    history_state: history::HistoryState,
    history_running: Option<NaiveDate>,
    history_retry: Option<Instant>,
    history_cancel: Option<Arc<AtomicBool>>,
    pub show_untracked: bool,
    pub discovery_days_ago: u32,
    empty_grid_port: Option<String>,
    pub(crate) grid_first_day: u32,
    pub(crate) grid_day_count: usize,
    day_selection: Option<(String, usize)>,
    discovery_revision: u64,
    inventoried_hosts: BTreeSet<String>,
    clock: Option<(NaiveDateTime, Instant)>,
    port_filters: BTreeMap<String, PortFilter>,
    pub include_only_marked_ports: bool,
    counter: Arc<AtomicU64>,
    pub host_bytes: BTreeMap<String, u64>,
    pub errors: BTreeMap<String, String>,
    service_health: service_health::ServiceHealth,
    pub errors_open: bool,
    pub error_scroll: u16,
    download_results: BTreeMap<(String, String), bool>,
    pub cache_root: PathBuf,
    pub busy: bool,
    pub download_queue_open: bool,
    pub download_queue_selected: usize,
    storage_busy: bool,
    cache_load: Option<cache_load::CacheLoadJob>,
    cache_load_needed: bool,
    activity: Option<LogActivity>,
    /// A discovery requested while a catch-up is running. It starts as soon
    /// as the current collector emits Done, so F/r never silently do nothing.
    refresh_queued: bool,
    demand_queued: bool,
    demand_files_queued: BTreeSet<String>,
    manual_downloads_queued: BTreeSet<String>,
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
        let (archive_tx, archive_rx) = mpsc::unbounded_channel();
        Self {
            files: Vec::new(),
            selected: 0,
            sidebar_kind: 0,
            files_focus: true,
            file_filter: String::new(),
            file_filter_editing: false,
            search: String::new(),
            search_editing: false,
            search_changed_at: None,
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
            selected_log_names: BTreeSet::new(),
            selection_across_days: false,
            rendered_day: None,
            preview_changed: None,
            sidebar_status: RefCell::new(None),
            sidebar_rows: RefCell::new(ui_updates::SidebarRows::default()),
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
            automatic_window: false,
            window_initialized: false,
            truncation_dialog: None,
            truncation_draft: None,
            truncation_sequence: None,
            truncation_dialog_opened_at: None,
            entry_detail: None,
            view_width: 0,
            rows: Vec::new(),
            host_views: Vec::new(),
            activity_graph: activity::ActivityGraph::default(),
            row_sources: Vec::new(),
            time_index: Vec::new(),
            rendered_records: RefCell::new(VecDeque::new()),
            first_time: None,
            mouse_regions: Vec::new(),
            environment_indicator: String::new(),
            host_detail_offset: 0,
            host_detail_capacity: 0,
            host_detail_count: 0,
            hosts_focus: false,
            host_selected: 0,
            excluded_hosts: BTreeSet::new(),
            host_pane_maximized: false,
            show_line_numbers: false,
            parsed_streams: HashMap::new(),
            retained_archive_days: HashMap::new(),
            parsed_large: HashMap::new(),
            page_navigation: None,
            index_filter_active: None,
            index_filter_target: None,
            index_filter_changed: None,
            index_filter_revision: 0,
            parse_jobs: HashMap::new(),
            read_failures: HashMap::new(),
            parse_lock: Arc::new(std::sync::Mutex::new(())),
            parse_tx,
            parse_rx,
            archive_job: None,
            archived_raw_cleanup: Vec::new(),
            archive_tx,
            archive_rx,
            archive_retry: HashMap::new(),
            archive_last_scan: None,
            archive_scan_day: None,
            archive_dirty: true,
            combined_dirty: false,
            server_offset: FixedOffset::east_opt(0).unwrap(),
            scope_days: 1,
            view_height: 0,
            extra_streams: BTreeSet::new(),
            disabled_streams: BTreeSet::new(),
            modes: BTreeMap::new(),
            tracking_periods: BTreeMap::new(),
            historical_tracking: BTreeMap::new(),
            history_checked: None,
            history_state: history::HistoryState::default(),
            history_running: None,
            history_retry: None,
            history_cancel: None,
            show_untracked: false,
            discovery_days_ago: 0,
            empty_grid_port: None,
            grid_first_day: 0,
            grid_day_count: 7,
            day_selection: None,
            discovery_revision: 0,
            inventoried_hosts: BTreeSet::new(),
            clock: None,
            port_filters: BTreeMap::new(),
            include_only_marked_ports: false,
            counter: Arc::new(AtomicU64::new(0)),
            host_bytes: BTreeMap::new(),
            errors: BTreeMap::new(),
            service_health: service_health::ServiceHealth::default(),
            errors_open: false,
            error_scroll: 0,
            download_results: BTreeMap::new(),
            cache_root: PathBuf::new(),
            busy: false,
            download_queue_open: false,
            download_queue_selected: 0,
            storage_busy: false,
            cache_load: None,
            cache_load_needed: false,
            activity: None,
            refresh_queued: false,
            demand_queued: false,
            demand_files_queued: BTreeSet::new(),
            manual_downloads_queued: BTreeSet::new(),
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
                let file = snapshot.open_content(root)?;
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
        if self.storage_busy {
            self.message = "Storage operation in progress.".into();
            return;
        }
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
            self.automatic_window = true;
            self.window_initialized = true;
        }
        if identity != self.identity {
            self.cancel();
            *self = Self {
                opened_from: self.opened_from.take(),
                window: self.window,
                undo_window: self.undo_window,
                default_truncation: self.default_truncation,
                automatic_window: self.automatic_window,
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
            self.cache_load_needed = true;
        }
        self.client = Some(client.clone());
        if self.cache_load.is_some() {
            return;
        }
        if self.cache_load_needed {
            self.start_cache_load();
            return;
        }
        self.discover_cached_hosts(client, rt);
    }

    fn discover_cached_hosts(&mut self, client: MarkLogicClient, rt: &Runtime) {
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
        self.refresh_automatic(rt);
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
        if same && (!self.cache_load_needed || self.cache_loading()) {
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
            staging: BTreeSet::new(),
            last: None,
            jobs: BTreeMap::new(),
            work_bytes: None,
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
                .map(|(host, file)| {
                    let phase = if activity.staging.contains(&(host.clone(), file.clone())) {
                        " (saving)"
                    } else {
                        ""
                    };
                    format!("{}:{file}{phase}", self.host_alias(host))
                })
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
        let bytes_label = if activity.work_bytes.is_some() {
            "copied"
        } else {
            "received"
        };
        Some(format!(
            "{spinner} {}{completed} · {} {bytes_label} · {}s{errors} · Q: queue · x: cancel",
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
        self.cache_load.take();
        if let Some(cancelled) = self.history_cancel.take() {
            cancelled.store(true, Ordering::Relaxed);
        }
        if self.history_running.take().is_some() {
            self.history_retry = Some(Instant::now() + Duration::from_secs(300));
        }
        if let Some(job) = self.archive_job.take() {
            self.archive_retry.insert(job.key.clone(), Instant::now());
        }
        while self.archive_rx.try_recv().is_ok() {}
        self.index_filter_changed = None;
        self.index_filter_target = self.index_filter_active.clone();
        if let Some(page) = &mut self.page_navigation {
            page.pending = None;
        }
        self.parse_jobs.clear();
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
        self.refresh_queued = false;
        self.demand_queued = false;
        self.demand_files_queued.clear();
        self.manual_downloads_queued.clear();
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
        self.load_cache_with_progress(None)?;
        self.cache_load_needed = false;
        Ok(())
    }

    fn load_cache_with_progress(&mut self, progress: Option<&cache_load::Progress>) -> Result<()> {
        self.archive_dirty = true;
        if let Some(progress) = progress {
            progress.stage(cache_load::Stage::Catalog)?;
        }
        self.invalidate_sidebar_status();
        self.files
            .retain(|file| !self.retained_archive_days.contains_key(&file.key()));
        self.retained_archive_days.clear();
        fs::create_dir_all(&self.cache_root)?;
        self.load_subscriptions();
        self.load_history_state();
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
        let mut known_files: BTreeSet<_> = self.files.iter().map(LogFile::key).collect();
        for entry in fs::read_dir(&self.cache_root)? {
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            if let Some(progress) = progress {
                progress.files.fetch_add(1, Ordering::Relaxed);
            }
            let mut snapshot: Snapshot = match serde_json::from_slice(&fs::read(&path)?) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let settled = snapshot.settled(self.server_offset);
            if snapshot.archive.is_some() && !settled {
                let valid = snapshot
                    .archive_path(&self.cache_root)
                    .and_then(|path| archive::ArchiveReader::open(&path).ok())
                    .is_some_and(|reader| reader.raw_len() == snapshot.text_bytes);
                if !valid {
                    // Prefer a still-present raw generation when its archive
                    // or sidecar is incomplete or has been damaged.
                    if fs::metadata(snapshot.content_path(&self.cache_root))
                        .is_ok_and(|meta| meta.len() >= snapshot.text_bytes)
                    {
                        snapshot.archive = None;
                    } else {
                        continue;
                    }
                }
            }
            if snapshot.storage.is_some()
                && !settled
                && snapshot.open_content(&self.cache_root).is_err()
            {
                continue;
            }
            if snapshot.text.is_empty() && snapshot.text_bytes > 0 {
                // Only the committed prefix counts: a crash between appending
                // content and writing the cursor must not leak uncommitted data.
                if settled {
                    // Settled generations are immutable local history. Validate
                    // and read their bytes only when a query actually needs them.
                } else if snapshot.text_bytes > BACKGROUND_PARSE_BYTES {
                    if snapshot.archive.is_none()
                        && snapshot.storage.is_none()
                        && !fs::metadata(snapshot.content_path(&self.cache_root))
                            .is_ok_and(|metadata| metadata.len() >= snapshot.text_bytes)
                    {
                        continue;
                    }
                } else {
                    let mut bytes = Vec::new();
                    let failed = index::open_source(&snapshot.read_path(&self.cache_root))
                        .and_then(|input| input.take(snapshot.text_bytes).read_to_end(&mut bytes))
                        .is_err()
                        || bytes.len() as u64 != snapshot.text_bytes;
                    let mut valid = !failed;
                    if failed && snapshot.archive.is_some() {
                        bytes.clear();
                        if fs::File::open(snapshot.content_path(&self.cache_root))
                            .and_then(|input| {
                                input.take(snapshot.text_bytes).read_to_end(&mut bytes)
                            })
                            .is_ok()
                            && bytes.len() as u64 == snapshot.text_bytes
                        {
                            snapshot.archive = None;
                            valid = true;
                        }
                    }
                    if !valid {
                        continue;
                    }
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
                snapshot.updated_at = fs::metadata(snapshot.read_path(&self.cache_root))
                    .and_then(|metadata| metadata.modified())
                    .or_else(|_| fs::metadata(&path).and_then(|metadata| metadata.modified()))
                    .ok()
                    .map(|time| DateTime::<Utc>::from(time).to_rfc3339());
            }
            let mut file = snapshot.file.clone();
            file.present = false;
            if known_files.insert(file.key()) {
                self.files.push(file);
            }
            self.snapshots.insert(snapshot.file.key(), snapshot);
        }
        if let Some(progress) = progress {
            progress.stage(cache_load::Stage::Archives)?;
        }
        self.restore_retained_archives(progress)?;
        if let Some(progress) = progress {
            progress.stage(cache_load::Stage::Preparing)?;
        }
        self.initialize_tracking_periods();
        self.prune_untracked_inventory();
        self.sort_files();
        self.rebuild_lines();
        self.scroll_to_bottom();
        Ok(())
    }

    /// Recover older compressed generations retained when a rotation name was reused.
    /// Their original metadata was replaced, but archive sidecars and bytes remain.
    fn restore_retained_archives(&mut self, progress: Option<&cache_load::Progress>) -> Result<()> {
        use std::io::{Seek, SeekFrom};
        let Ok(entries) = fs::read_dir(self.cache_root.join("archives")) else {
            return Ok(());
        };
        let originals: HashMap<String, Snapshot> = self
            .snapshots
            .values()
            .map(|snapshot| (hash_key(&snapshot.file.key()), snapshot.meta()))
            .collect();
        let cached = fs::read(self.cache_root.join(history::RETAINED_FILE))
            .ok()
            .and_then(|data| serde_json::from_slice::<history::RetainedCatalog>(&data).ok())
            .filter(|catalog| catalog.offset == self.server_offset.local_minus_utc());
        let mut remembered = history::RetainedCatalog {
            offset: self.server_offset.local_minus_utc(),
            ..Default::default()
        };
        let mut seen: BTreeSet<_> = self
            .snapshots
            .values()
            .filter_map(|snapshot| snapshot.archive.as_deref().map(history::archive_identity))
            .collect();
        let mut catalog_changed = cached.is_none();
        for entry in entries.flatten() {
            if let Some(progress) = progress {
                progress.check_cancelled()?;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".gz")
                || name.len() <= 65
                || !name.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
            {
                continue;
            }
            let Some(original) = originals.get(&name[..64]) else {
                continue;
            };
            if original.archive.as_deref() == Some(name.as_str()) {
                continue;
            }
            let identity = history::archive_identity(&name);
            if seen.contains(&identity) {
                continue;
            }
            if let Some(saved) = cached
                .as_ref()
                .and_then(|catalog| catalog.entries.get(&name))
                .filter(|saved| {
                    saved.snapshot.archive.as_deref() == Some(name.as_str())
                        && saved.snapshot.file.host == original.file.host
                })
            {
                seen.insert(identity);
                let snapshot = saved.snapshot.meta();
                let key = snapshot.file.key();
                self.retained_archive_days
                    .insert(key.clone(), saved.days.clone());
                self.files.push(snapshot.file.clone());
                remembered.entries.insert(
                    name.clone(),
                    history::RetainedEntry {
                        snapshot: snapshot.meta(),
                        days: saved.days.clone(),
                    },
                );
                self.snapshots.insert(key, snapshot);
                continue;
            }
            let Ok(mut reader) = archive::ArchiveReader::open(&entry.path()) else {
                continue;
            };
            catalog_changed = true;
            let size = reader.raw_len();
            if size == 0 {
                continue;
            }
            // Rotation archives are typically a single day; inspect both ends so
            // spanning files remain discoverable without reading entire archives.
            let mut samples = Vec::new();
            let mut head = vec![0; size.min(65536) as usize];
            if reader.read_exact(&mut head).is_err() {
                continue;
            }
            samples.push(head);
            if size > 65536 && reader.seek(SeekFrom::End(-65536)).is_ok() {
                let mut tail = vec![0; 65536];
                if reader.read_exact(&mut tail).is_ok() {
                    samples.push(tail);
                }
            }
            let dates: Vec<_> = samples
                .iter()
                .flat_map(|sample| {
                    String::from_utf8_lossy(sample)
                        .lines()
                        .filter_map(|line| {
                            presentation::line_time(&original.file, line, self.server_offset)
                                .map(|time| time.date())
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
            let (Some(first), Some(last)) = (dates.iter().min(), dates.iter().max()) else {
                continue;
            };
            let span = (*last - *first).num_days();
            if !(0..=3650).contains(&span) {
                continue;
            }
            let days = (0..=span).map(|day| *first + Days::days(day)).collect();
            let mut snapshot = original.clone();
            snapshot.file.path = format!("{}#retained-{}", original.file.path, name);
            snapshot.file.present = false;
            snapshot.file.size = size;
            snapshot.archive = Some(name.clone());
            snapshot.storage = None;
            snapshot.text_bytes = size;
            snapshot.text.clear();
            snapshot.full_size = Some(size);
            if size <= BACKGROUND_PARSE_BYTES && !snapshot.settled(self.server_offset) {
                if reader.seek(SeekFrom::Start(0)).is_err()
                    || reader.read_to_string(&mut snapshot.text).is_err()
                {
                    continue;
                }
            }
            let key = snapshot.file.key();
            seen.insert(identity);
            self.retained_archive_days.insert(key.clone(), days);
            self.files.push(snapshot.file.clone());
            remembered.entries.insert(
                name,
                history::RetainedEntry {
                    snapshot: snapshot.meta(),
                    days: self.retained_archive_days[&key].clone(),
                },
            );
            self.snapshots.insert(key, snapshot);
        }
        if catalog_changed
            || cached
                .as_ref()
                .is_none_or(|catalog| catalog.entries.keys().ne(remembered.entries.keys()))
        {
            atomic_write(
                &self.cache_root.join(history::RETAINED_FILE),
                &serde_json::to_vec(&remembered)?,
            )?;
        }
        Ok(())
    }

    fn initialize_tracking_periods(&mut self) {
        let today = self.now().date();
        for name in self.tracked_names() {
            if self.tracking_periods.contains_key(&name) {
                continue;
            }
            let start = self
                .snapshots
                .values()
                .filter(|snapshot| Self::stream_name(&snapshot.file.name) == name)
                .filter_map(|snapshot| {
                    snapshot
                        .updated_at
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .map(|time| time.with_timezone(&self.server_offset).date_naive())
                })
                .min()
                .unwrap_or(today)
                .min(today);
            self.tracking_periods
                .insert(name, vec![TrackingPeriod { start, end: None }]);
        }
        self.save_subscriptions();
    }

    /// Publish an archive already verified by the archive worker. The raw
    /// generation stays in place for existing readers and recovery.
    fn publish_archive(
        &mut self,
        key: &str,
        generation: u64,
        bytes: u64,
        name: String,
    ) -> Result<bool> {
        let snapshot = self
            .snapshots
            .get(key)
            .context("Cached generation disappeared")?;
        if snapshot.generation != generation
            || snapshot.text_bytes != bytes
            || snapshot.full_size != Some(bytes)
            || snapshot.storage.is_some()
        {
            bail!("Cached generation changed or is not a complete download");
        }
        let mut meta = snapshot.meta();
        meta.archive = Some(name);
        let archive = meta
            .archive_path(&self.cache_root)
            .context("Invalid archive path")?;
        let reader = archive::ArchiveReader::open(&archive)?;
        if reader.raw_len() != bytes {
            bail!("Archive size does not match the cached generation");
        }
        let _ = fs::hard_link(
            snapshot.index_path(&self.cache_root),
            meta.index_path(&self.cache_root),
        );
        fs::File::open(archive.parent().context("No archive directory")?)?.sync_all()?;
        let metadata_path = self.cache_root.join(format!("{}.json", hash_key(key)));
        let temp_path = metadata_path.with_extension("tmp");
        let mut output = fs::File::create(&temp_path)?;
        output.write_all(&serde_json::to_vec(&meta)?)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temp_path, &metadata_path)?;
        // If the directory cannot be synced, retain both copies even though
        // the archive reference is already visible in this process.
        let durable = fs::File::open(&self.cache_root)
            .and_then(|directory| directory.sync_all())
            .is_ok();
        self.snapshots.get_mut(key).unwrap().archive = meta.archive;
        Ok(durable)
    }

    pub fn archive_status(&self) -> Option<String> {
        self.archive_job
            .as_ref()
            .map(|job| format!("Archiving {} · x: cancel", job.name))
    }

    fn cleanup_archived_raw(&mut self) {
        let jobs = &self.parse_jobs;
        let snapshots = &self.snapshots;
        self.archived_raw_cleanup.retain_mut(|old| {
            if jobs
                .get(&old.key)
                .is_some_and(|job| job.spec.generation == old.generation)
                || Instant::now() < old.retry_at
            {
                return true;
            }
            if snapshots.get(&old.key).is_some_and(|snapshot| {
                snapshot.generation == old.generation
                    && (snapshot.archive.is_none() || snapshot.text_bytes != old.bytes)
            }) {
                return false; // This raw generation became writable again.
            }
            if let Some(snapshot) = snapshots.get(&old.key).filter(|snapshot| {
                snapshot.generation == old.generation && snapshot.archive.is_some()
            }) {
                let _ = fs::hard_link(
                    self.cache_root
                        .join(format!("{}.records.sqlite", hash_key(&old.key))),
                    snapshot.index_path(&self.cache_root),
                );
            }
            match fs::remove_file(&old.path) {
                Ok(()) => false,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
                Err(_) => {
                    old.retry_at = Instant::now() + READ_RETRY_INTERVAL;
                    true
                }
            }
        });
    }

    fn tick_archive(&mut self, rt: &Runtime) {
        self.cleanup_archived_raw();
        if let Ok(mut completed) = self.archive_rx.try_recv() {
            let current = self.archive_job.as_ref().is_some_and(|job| {
                job.key == completed.key && Arc::ptr_eq(&job.cancelled, &completed.cancelled)
            });
            if current {
                let display_name = self.archive_job.as_ref().unwrap().name.clone();
                self.archive_job.take();
                self.archive_last_scan = None;
                self.archive_dirty = true;
                match completed.result.as_mut() {
                    Ok(artifact) => {
                        let lock = ArchivePublishLock::acquire(&self.cache_root);
                        let result = (|| -> Result<bool> {
                            if self.storage_busy {
                                bail!("Another storage operation is using this cache");
                            }
                            let _held = lock
                                .as_ref()
                                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                            if fs::read(
                                self.cache_root
                                    .join(format!("{}.json", hash_key(&completed.key))),
                            )? != completed.metadata
                            {
                                bail!("Cached generation changed during archiving");
                            }
                            self.publish_archive(
                                &completed.key,
                                completed.generation,
                                completed.bytes,
                                completed.name.clone(),
                            )
                        })();
                        match result {
                            Ok(durable) => {
                                self.archive_retry.remove(&completed.key);
                                artifact.keep = true;
                                let raw =
                                    self.snapshots[&completed.key].content_path(&self.cache_root);
                                self.message = if !durable {
                                    format!(
                                        "Archived {}; original retained until cache metadata is durable.",
                                        display_name
                                    )
                                } else if self
                                    .parse_jobs
                                    .get(&completed.key)
                                    .is_some_and(|job| job.spec.generation == completed.generation)
                                {
                                    // Storage format changed, not content. Let the
                                    // pinned reader finish against the original file.
                                    self.archived_raw_cleanup.push(ArchivedRawCleanup {
                                        key: completed.key.clone(),
                                        generation: completed.generation,
                                        bytes: completed.bytes,
                                        path: raw,
                                        retry_at: Instant::now(),
                                    });
                                    format!("Archived {display_name}; finishing local read.")
                                } else {
                                    match fs::remove_file(&raw) {
                                        Ok(()) => format!("Archived {display_name}."),
                                        Err(error) => format!(
                                            "Archived {}; original retained: {error}",
                                            display_name
                                        ),
                                    }
                                };
                            }
                            Err(error) => {
                                self.archive_retry
                                    .insert(completed.key.clone(), Instant::now());
                                self.message = format!("Archiving deferred: {error:#}");
                            }
                        }
                    }
                    Err(error) => {
                        self.archive_retry
                            .insert(completed.key.clone(), Instant::now());
                        self.message = format!("Archiving deferred: {error}");
                    }
                }
            }
        }
        if self.storage_busy
            || self.busy
            || self.archive_job.is_some()
            || self.cache_root.as_os_str().is_empty()
        {
            return;
        }
        if !self.archive_dirty
            && self.archive_scan_day == Some(self.now().date())
            && !self
                .archive_retry
                .values()
                .any(|time| time.elapsed() >= Duration::from_secs(300))
        {
            return;
        }
        if self
            .archive_last_scan
            .is_some_and(|last| last.elapsed() < STATUS_INTERVAL)
        {
            return;
        }
        self.archive_last_scan = Some(Instant::now());
        self.archive_scan_day = Some(self.now().date());
        self.archive_dirty = false;
        let cutoff = self.now() - Days::days(2);
        let candidate = self.snapshots.iter().find_map(|(key, snapshot)| {
            let modified = DateTime::parse_from_rfc3339(&snapshot.file.modified)
                .ok()?
                .with_timezone(&self.server_offset)
                .naive_local();
            let rotated = Self::file_days_ago(&snapshot.file).is_some_and(|day| day > 0);
            let retry = self
                .archive_retry
                .get(key)
                .is_some_and(|time| time.elapsed() < Duration::from_secs(300));
            let mode = self.tracking_mode(&snapshot.file);
            let settled = snapshot.settled(self.server_offset);
            let eligible = rotated
                && !self.parse_jobs.contains_key(key)
                && snapshot.text_bytes > 0
                && (settled || modified < cutoff)
                && !retry
                && !matches!(mode, TrackingMode::Live | TrackingMode::Partial)
                && snapshot.archive.is_none()
                && snapshot.storage.is_none()
                && snapshot.full_size == Some(snapshot.text_bytes)
                && snapshot.file.size == snapshot.text_bytes
                && snapshot.start == 0
                && snapshot.end == snapshot.length;
            let endpoint = if settled {
                None
            } else {
                if !self.automatic_collection_allowed() {
                    return None;
                }
                Some(
                    self.endpoints
                        .iter()
                        .find(|endpoint| endpoint.host == snapshot.file.host)?
                        .clone(),
                )
            };
            eligible.then(|| (key.clone(), snapshot.meta(), endpoint))
        });
        let Some((key, snapshot, endpoint)) = candidate else {
            return;
        };
        let metadata_path = self.cache_root.join(format!("{}.json", hash_key(&key)));
        let metadata = match fs::read(metadata_path) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.archive_retry.insert(key, Instant::now());
                return;
            }
        };
        let valid = serde_json::from_slice::<Snapshot>(&metadata).is_ok_and(|disk| {
            disk.file.key() == key
                && disk.file.size == snapshot.file.size
                && disk.file.modified == snapshot.file.modified
                && disk.generation == snapshot.generation
                && disk.text_bytes == snapshot.text_bytes
                && disk.full_size == snapshot.full_size
                && disk.archive.is_none()
                && disk.storage.is_none()
        });
        if !valid {
            self.archive_retry.insert(key, Instant::now());
            return;
        }
        static NEXT_ARCHIVE: AtomicU64 = AtomicU64::new(0);
        let name = format!(
            "{}-{}-{}-{}.gz",
            hash_key(&key),
            snapshot.generation,
            std::process::id(),
            NEXT_ARCHIVE.fetch_add(1, Ordering::Relaxed)
        );
        let destination = self.cache_root.join("archives").join(&name);
        let raw = snapshot.content_path(&self.cache_root);
        let directory = self.directory.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        self.archive_job = Some(ArchiveJob {
            key: key.clone(),
            name: snapshot.file.name.clone(),
            cancelled,
        });
        let tx = self.archive_tx.clone();
        rt.spawn(async move {
            let result = async {
                if let Some(endpoint) = &endpoint {
                    archive_remote_unchanged(endpoint, &directory, &snapshot.file).await?;
                }
                if worker_cancelled.load(Ordering::Relaxed) {
                    bail!("Archive cancelled");
                }
                let before = fs::metadata(&raw)?;
                if before.len() != snapshot.text_bytes {
                    bail!("Cached file changed");
                }
                let before_modified = before.modified()?;
                let stop = worker_cancelled.clone();
                let dest = destination.clone();
                let input = raw.clone();
                let bytes = snapshot.text_bytes;
                let artifact = tokio::task::spawn_blocking(move || -> Result<ArchiveArtifact> {
                    fs::create_dir_all(dest.parent().context("No archive directory")?)?;
                    archive::build(&input, &dest, bytes, &stop)?;
                    Ok(ArchiveArtifact {
                        path: dest,
                        keep: false,
                    })
                })
                .await??;
                let after = fs::metadata(&raw)?;
                if after.len() != snapshot.text_bytes || after.modified()? != before_modified {
                    bail!("Cached file changed during archiving");
                }
                if let Some(endpoint) = &endpoint {
                    archive_remote_unchanged(endpoint, &directory, &snapshot.file).await?;
                }
                if worker_cancelled.load(Ordering::Relaxed) {
                    bail!("Archive cancelled");
                }
                Ok(artifact)
            }
            .await
            .map_err(|error: anyhow::Error| format!("{error:#}"));
            let _ = tx.send(ArchiveResult {
                key,
                generation: snapshot.generation,
                bytes: snapshot.text_bytes,
                name,
                cancelled: worker_cancelled,
                metadata,
                result,
            });
        });
    }

    fn prepare_stream(&mut self, key: &str) -> bool {
        self.sidebar_rows.borrow_mut().clear();
        let window = self.display_window();
        let Some(snapshot) = self.snapshots.get(key) else {
            return false;
        };
        if snapshot.text_bytes == 0 || !snapshot.text.is_empty() {
            self.parsed_streams
                .entry(key.to_owned())
                .or_default()
                .update(&snapshot.file, &snapshot.text, false, self.server_offset);
            return true;
        }
        let paged = !self.combined
            && !self.all_cached
            && self
                .files
                .iter()
                .filter(|file| {
                    file.name == snapshot.file.name
                        && self
                            .snapshots
                            .get(&file.key())
                            .is_some_and(|s| s.text_bytes > 0)
                })
                .count()
                == 1;
        let spec = ParseSpec {
            generation: snapshot.generation,
            bytes: snapshot.text_bytes,
            offset: self.server_offset,
            window,
            paged,
            filter_id: if paged && self.index_filter_active.is_some() {
                self.index_filter_revision
            } else {
                0
            },
            before: if self.combined || self.all_cached {
                None
            } else {
                self.page_navigation
                    .as_ref()
                    .filter(|page| {
                        page.key == key
                            && page.generation == snapshot.generation
                            && page.window == window
                    })
                    .and_then(|page| {
                        page.pending.as_ref().map_or_else(
                            || page.cursors.last().copied().flatten(),
                            |(cursors, _)| cursors.last().copied().flatten(),
                        )
                    })
            },
        };
        let ready_prefix = self.parsed_streams.contains_key(key)
            && self
                .parsed_large
                .get(key)
                .is_some_and(|(ready, _)| ready.serves_prefix_of(spec));
        if ready_prefix
            && self
                .parsed_large
                .get(key)
                .is_some_and(|(ready, _)| *ready == spec)
        {
            return true;
        }
        if self
            .parse_jobs
            .get(key)
            .is_some_and(|job| job.spec.serves_prefix_of(spec))
        {
            // Appends can arrive faster than a large read finishes. Complete
            // the pinned prefix first instead of cancelling it on every poll.
            return ready_prefix;
        }
        if self.read_failures.get(key).is_some_and(|failure| {
            failure.spec.serves_prefix_of(spec) && failure.at.elapsed() < READ_RETRY_INTERVAL
        }) {
            return ready_prefix;
        }
        let changing_filter = self
            .parsed_large
            .get(key)
            .is_some_and(|(old, _)| old.filter_id != spec.filter_id);
        let changing_page = self
            .page_navigation
            .as_ref()
            .is_some_and(|page| page.key == key && page.pending.is_some());
        if !changing_page && !changing_filter && !ready_prefix {
            self.parsed_streams.remove(key);
            self.parsed_large.remove(key);
        }
        self.parse_jobs.remove(key);
        let progress = Arc::new(AtomicU64::new(0));
        let cancelled = Arc::new(AtomicBool::new(false));
        let phase = Arc::new(AtomicU8::new(index::ReadPhase::Queued as u8));
        let path = snapshot.read_path(&self.cache_root);
        let raw_fallback = snapshot
            .archive
            .as_ref()
            .map(|_| snapshot.content_path(&self.cache_root));
        let matcher = if spec.filter_id == 0 {
            None
        } else {
            self.index_filter_active
                .as_deref()
                .and_then(filter::IndexedQuery::from_signature)
        };
        let index_path = snapshot.index_path(&self.cache_root);
        let file = snapshot.file.clone();
        let tx = self.parse_tx.clone();
        let task_key = key.to_owned();
        let task_progress = progress.clone();
        let task_cancelled = cancelled.clone();
        let task_phase = phase.clone();
        let lock = self.parse_lock.clone();
        std::thread::spawn(move || {
            let _permit = lock.lock().unwrap_or_else(|error| error.into_inner());
            if task_cancelled.load(Ordering::Relaxed) {
                return;
            }
            let mut result = index::read_with_phase(
                &path,
                &index_path,
                &file,
                spec,
                &task_progress,
                &task_cancelled,
                matcher.as_ref(),
                Some(&task_phase),
            );
            let mut fallback_reason = None;
            if result
                .as_ref()
                .is_err_and(|error| error.kind() != std::io::ErrorKind::Interrupted)
                && let Some(raw) = raw_fallback.as_ref().filter(|raw| {
                    fs::metadata(raw).is_ok_and(|metadata| metadata.len() >= spec.bytes)
                })
            {
                fallback_reason =
                    Some("Archive unavailable; using the original cached file.".to_owned());
                task_progress.store(0, Ordering::Relaxed);
                result = index::read_with_phase(
                    raw,
                    &index_path,
                    &file,
                    spec,
                    &task_progress,
                    &task_cancelled,
                    matcher.as_ref(),
                    Some(&task_phase),
                );
            }
            let result = match result {
                Err(error)
                    if error.kind() != std::io::ErrorKind::Interrupted
                        && spec.before.is_none()
                        && spec.filter_id == 0 =>
                {
                    fallback_reason = Some(format!(
                        "Index unavailable ({error}); using a full background scan."
                    ));
                    task_progress.store(0, Ordering::Relaxed);
                    task_phase.store(index::ReadPhase::Scanning as u8, Ordering::Relaxed);
                    let scan_path = raw_fallback
                        .as_ref()
                        .filter(|raw| {
                            fs::metadata(raw).is_ok_and(|metadata| metadata.len() >= spec.bytes)
                        })
                        .unwrap_or(&path);
                    index::open_source(scan_path).and_then(|input| {
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
                    })
                }
                result => result,
            };
            let _ = tx.send(ParseResult {
                key: task_key,
                spec,
                progress: task_progress,
                fallback_reason,
                result,
            });
        });
        self.parse_jobs.insert(
            key.to_owned(),
            ParseJob {
                spec,
                progress,
                cancelled,
                phase,
            },
        );
        ready_prefix
    }

    fn can_query_index(&self) -> bool {
        !self.combined
            && !self.all_cached
            && (self.search.is_empty() || !self.options.relative || self.options.raw)
            && self.context_before == 0
            && self.context_after == 0
            && self.selected_file().is_some_and(|file| {
                self.host_enabled(&file.host)
                    && self
                        .snapshots
                        .get(&file.key())
                        .is_some_and(|s| s.text_bytes > BACKGROUND_PARSE_BYTES && s.text.is_empty())
                    && self
                        .files
                        .iter()
                        .filter(|f| {
                            f.name == file.name
                                && self.host_enabled(&f.host)
                                && self
                                    .snapshots
                                    .get(&f.key())
                                    .is_some_and(|s| s.text_bytes > 0)
                        })
                        .count()
                        == 1
            })
    }

    pub fn search_changed(&mut self) {
        if self.can_query_index() {
            self.schedule_index_filter();
        } else {
            self.search_changed_at = Some(Instant::now());
        }
    }

    fn schedule_index_filter(&mut self) {
        let eligible =
            self.can_query_index() && (self.content_filters.active() || !self.search.is_empty());
        let target = eligible.then(|| self.content_filters.signature(&self.search, &self.options));
        if target != self.index_filter_target {
            self.index_filter_target = target;
            self.index_filter_changed = Some(Instant::now());
            if let Some(key) = self.selected_file().map(LogFile::key) {
                self.parse_jobs.remove(&key);
            }
        }
    }

    fn tick_index_filter(&mut self) {
        if !self
            .index_filter_changed
            .is_some_and(|t| t.elapsed() >= Duration::from_millis(300))
        {
            return;
        }
        self.index_filter_changed = None;
        if self.index_filter_active == self.index_filter_target {
            return;
        }
        self.index_filter_active = self.index_filter_target.clone();
        self.index_filter_revision = self.index_filter_revision.wrapping_add(1).max(1);
        self.page_navigation = None;
        if let Some(key) = self.selected_file().map(LogFile::key) {
            self.message = "Scanning indexed log for matching records…".into();
            self.prepare_stream(&key);
        }
    }

    /// Page by source sequence, retaining the visible page until the next is ready.
    /// Multi-source views require a shared chronological cursor and do not use this path.
    pub fn page_large_source(&mut self, older: bool) -> bool {
        let window = self.display_window();
        if self.combined || self.all_cached {
            return false;
        }
        let Some(file) = self.selected_file().cloned() else {
            return false;
        };
        if !self.host_enabled(&file.host) {
            return false;
        }
        if self
            .files
            .iter()
            .filter(|f| {
                f.name == file.name
                    && self.host_enabled(&f.host)
                    && self
                        .snapshots
                        .get(&f.key())
                        .is_some_and(|s| s.text_bytes > 0)
            })
            .count()
            != 1
        {
            return false;
        }
        let key = file.key();
        let Some(snapshot) = self.snapshots.get(&key) else {
            return false;
        };
        let generation = snapshot.generation;
        let committed_bytes = snapshot.text_bytes;
        let Some((ready, limited)) = self.parsed_large.get(&key).copied() else {
            return false;
        };
        if ready.generation != generation
            || ready.window != window
            || self.parse_jobs.contains_key(&key)
        {
            return false;
        }
        let Some(stream) = self.parsed_streams.get(&key) else {
            return false;
        };
        let (Some(first), Some(last)) = (stream.index_first, stream.index_last) else {
            return false;
        };
        let page = self.page_navigation.get_or_insert_with(|| PageNavigation {
            key: key.clone(),
            generation,
            window,
            cursors: vec![None],
            pinned_bytes: committed_bytes,
            pending: None,
        });
        if page.key != key || page.generation != generation || page.window != window {
            *page = PageNavigation {
                key: key.clone(),
                generation,
                window,
                cursors: vec![None],
                pinned_bytes: committed_bytes,
                pending: None,
            };
        }
        let mut next = page.cursors.clone();
        if older {
            if !limited {
                return false;
            }
            // Freeze the previous newest page before navigating away, so
            // later appends cannot shift its start and create a gap.
            if next.len() == 1 && next[0].is_none() {
                let Some(end) = last.checked_add(1) else {
                    return false;
                };
                next[0] = Some(end);
                page.pinned_bytes = ready.bytes;
            }
            next.push(Some(first));
        } else if next.len() > 1 {
            next.pop();
        } else if next[0].is_some() && committed_bytes > page.pinned_bytes {
            next[0] = None;
        } else {
            return false;
        }
        page.pending = Some((next, older));
        self.following = false;
        self.message = if older {
            "Loading older log records…"
        } else {
            "Loading newer log records…"
        }
        .into();
        self.prepare_stream(&key);
        true
    }

    pub fn indexed_filter_completed(&self) -> bool {
        self.selected_file().is_some_and(|file| {
            let key = file.key();
            self.index_filter_changed.is_none()
                && !self.parse_jobs.contains_key(&key)
                && self.parsed_large.get(&key).is_some_and(|(spec, _)| {
                    spec.filter_id != 0 && spec.filter_id == self.index_filter_revision
                })
        })
    }

    pub fn indexed_page_status(&self) -> Option<String> {
        if self.combined || self.all_cached {
            return None;
        }
        let file = self.selected_file()?;
        if self
            .files
            .iter()
            .filter(|f| {
                f.name == file.name
                    && self
                        .snapshots
                        .get(&f.key())
                        .is_some_and(|s| s.text_bytes > 0)
            })
            .count()
            != 1
        {
            return None;
        }
        let key = file.key();
        let (spec, limited) = self.parsed_large.get(&key)?;
        self.parsed_streams.get(&key)?.index_first?;
        let page = self
            .page_navigation
            .as_ref()
            .filter(|p| p.key == key && p.generation == spec.generation && p.window == spec.window);
        let depth = page.map_or(0, |p| p.cursors.len().saturating_sub(1));
        let newer = depth > 0
            || page.is_some_and(|p| {
                p.cursors.first().is_some_and(Option::is_some) && spec.bytes > p.pinned_bytes
            });
        if !limited && !newer {
            return None;
        }
        Some(format!(
            "Indexed page {} · {}{}{} · , older / . newer",
            depth + 1,
            if *limited {
                "older available"
            } else {
                "oldest"
            },
            if newer { " · newer available" } else { "" },
            if spec.filter_id != 0 {
                " · matching records"
            } else if !self.search.is_empty() || self.content_filter_active() {
                " · filtering this page"
            } else {
                ""
            }
        ))
    }

    pub fn parsing_status(&self) -> Option<String> {
        if self.parse_jobs.is_empty() {
            return None;
        }
        let jobs: Vec<_> = self
            .parse_jobs
            .iter()
            .map(|(key, job)| (key, job, job.phase.load(Ordering::Relaxed)))
            .collect();
        let queued = jobs
            .iter()
            .filter(|(_, _, phase)| *phase == index::ReadPhase::Queued as u8)
            .count();
        if let Some((key, job, phase)) = jobs.iter().find(|(_, _, phase)| {
            *phase == index::ReadPhase::Indexing as u8 || *phase == index::ReadPhase::Scanning as u8
        }) {
            let percent = if job.spec.bytes == 0 {
                0
            } else {
                u128::from(job.progress.load(Ordering::Relaxed).min(job.spec.bytes)) * 100
                    / u128::from(job.spec.bytes)
            };
            let source = self
                .snapshots
                .get(*key)
                .map(|snapshot| format!("{} / {}", snapshot.file.host, snapshot.file.name))
                .unwrap_or_else(|| "local log".into());
            let action = if *phase == index::ReadPhase::Indexing as u8 {
                "Indexing"
            } else {
                "Scanning"
            };
            return Some(format!(
                "{action} {source}: {percent}% · {queued} queued · x: cancel"
            ));
        }
        let reading = jobs
            .iter()
            .filter(|(_, _, phase)| *phase == index::ReadPhase::Reading as u8)
            .count();
        let action = if reading > 0 {
            if jobs.iter().any(|(key, job, _)| {
                self.parsed_large
                    .get(*key)
                    .is_some_and(|(ready, _)| ready.serves_prefix_of(job.spec))
            }) {
                "Updating indexed log records"
            } else {
                "Loading indexed log records"
            }
        } else if queued == jobs.len() {
            "Waiting for local log reader"
        } else {
            "Opening local log indexes"
        };
        Some(format!(
            "{action} · {} file(s), {queued} queued · x: cancel",
            jobs.len()
        ))
    }

    pub fn large_view_limited(&self) -> bool {
        let window = self.display_window();
        let selected = self.selected_file();
        self.parsed_large.iter().any(|(key, (spec, limited))| {
            *limited
                && spec.window == window
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
            #[serde(default)]
            periods: BTreeMap<String, Vec<TrackingPeriod>>,
            #[serde(default)]
            historical: BTreeMap<String, BTreeMap<NaiveDate, bool>>,
        }
        if let Ok(bytes) = fs::read(self.cache_root.join(SUBSCRIPTIONS_FILE))
            && let Ok(saved) = serde_json::from_slice::<Saved>(&bytes)
        {
            self.historical_tracking = saved.historical;
            let today = self.now().date();
            let mut legacy = BTreeMap::new();
            for name in &saved.extra {
                legacy.insert(name.clone(), true);
            }
            for name in &saved.disabled {
                legacy.insert(name.clone(), false);
            }
            for (name, mode) in &saved.modes {
                legacy.insert(name.clone(), *mode != TrackingMode::Untracked);
            }
            for (name, tracked) in legacy {
                if let Some(age) = Self::name_days_ago(&name).filter(|age| *age > 0)
                    && let Some(day) = today.checked_sub_signed(Days::days(i64::from(age)))
                {
                    self.historical_tracking
                        .entry(Self::stream_name(&name))
                        .or_default()
                        .entry(day)
                        .or_insert(tracked);
                }
            }
            self.extra_streams = saved
                .extra
                .into_iter()
                .filter(|name| Self::name_days_ago(name) == Some(0))
                .collect();
            self.disabled_streams = saved
                .disabled
                .into_iter()
                .filter(|name| Self::name_days_ago(name) == Some(0))
                .collect();
            for (name, mode) in saved.modes {
                if Self::name_days_ago(&name) == Some(0) {
                    self.modes.insert(name, mode);
                }
            }
            self.tracking_periods = saved.periods;
        }
    }

    fn save_subscriptions(&mut self) {
        self.invalidate_sidebar_status();
        let value = json!({"extra":self.extra_streams, "disabled":self.disabled_streams, "modes": self.modes, "periods": self.tracking_periods, "historical": self.historical_tracking});
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

    fn same_modified(left: &str, right: &str) -> bool {
        match (
            DateTime::parse_from_rfc3339(left),
            DateTime::parse_from_rfc3339(right),
        ) {
            (Ok(left), Ok(right)) => left == right,
            _ => left == right,
        }
    }

    fn stream_name(name: &str) -> String {
        let stem = name.strip_suffix(".txt").unwrap_or(name);
        let base = stem
            .rsplit_once('_')
            .filter(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
            .map_or(stem, |(base, _)| base);
        format!("{base}.txt")
    }

    pub fn tracking_mode(&self, file: &LogFile) -> TrackingMode {
        let name = Self::stream_name(&file.name);
        let active = self.mode_for_name(&name);
        let age = Self::file_days_ago(file);
        if age == Some(0) {
            return active;
        }
        let day = age.and_then(|age| {
            self.now()
                .date()
                .checked_sub_signed(Days::days(i64::from(age)))
        });
        if let Some(tracked) = day.and_then(|day| self.historical_tracking.get(&name)?.get(&day)) {
            return if *tracked {
                TrackingMode::OnDemand
            } else {
                TrackingMode::Untracked
            };
        }
        if let (Some(periods), Some(day)) = (self.tracking_periods.get(&name), day) {
            if !periods
                .iter()
                .any(|period| period.start <= day && period.end.is_none_or(|end| day <= end))
            {
                return TrackingMode::Untracked;
            }
            return TrackingMode::OnDemand;
        }
        if active == TrackingMode::Untracked {
            TrackingMode::Untracked
        } else {
            TrackingMode::OnDemand
        }
    }

    fn mode_for_name(&self, name: &str) -> TrackingMode {
        let name = Self::stream_name(name);
        if let Some(mode) = self.modes.get(&name) {
            return *mode;
        }
        if !self.disabled_streams.contains(&name)
            && (DEFAULT_STREAMS.contains(&name.as_str()) || self.extra_streams.contains(&name))
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
        let mode = if Self::file_days_ago(&file).is_some_and(|day| day > 0) {
            if self.tracking_mode(&file) == TrackingMode::Untracked {
                TrackingMode::OnDemand
            } else {
                TrackingMode::Untracked
            }
        } else {
            match self.tracking_mode(&file) {
                TrackingMode::Untracked => TrackingMode::OnDemand,
                TrackingMode::OnDemand => TrackingMode::Live,
                TrackingMode::Live => TrackingMode::Partial,
                TrackingMode::Partial => TrackingMode::OnDemand,
            }
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

    pub fn toggle_tracking_selected(&mut self) {
        let Some(file) = self.selected_file().cloned() else {
            return;
        };
        let mode = if self.tracking_mode(&file) == TrackingMode::Untracked {
            TrackingMode::OnDemand
        } else {
            TrackingMode::Untracked
        };
        self.set_selected_mode(file, mode);
    }

    fn set_selected_mode(&mut self, file: LogFile, mode: TrackingMode) {
        let name = Self::stream_name(&file.name);
        if let Some(age) = Self::file_days_ago(&file).filter(|age| *age > 0) {
            let Some(day) = self
                .now()
                .date()
                .checked_sub_signed(Days::days(i64::from(age)))
            else {
                self.message = "The selected log day is out of range.".into();
                return;
            };
            let tracked = mode != TrackingMode::Untracked;
            self.historical_tracking
                .entry(name)
                .or_default()
                .insert(day, tracked);
            if !tracked {
                self.forced_downloads_queued.remove(&file.name);
            }
            self.save_subscriptions();
            self.sort_files();
            if self.combined {
                self.rebuild_lines();
            }
            self.ensure_visible_selection();
            self.message = if tracked {
                format!(
                    "{}: tracked for full on-demand download on every host; press d to download.",
                    file.name
                )
            } else {
                format!(
                    "{}: untracked on every host; cached content kept.",
                    file.name
                )
            };
            return;
        }
        let previously_active = self.mode_for_name(&name) != TrackingMode::Untracked;
        let now_active = mode != TrackingMode::Untracked;
        let today = self.now().date();
        if previously_active && !self.tracking_periods.contains_key(&name) {
            self.tracking_periods.insert(
                name.clone(),
                vec![TrackingPeriod {
                    start: today,
                    end: None,
                }],
            );
        }
        if previously_active != now_active {
            let periods = self.tracking_periods.entry(name.clone()).or_default();
            if now_active {
                periods.push(TrackingPeriod {
                    start: today,
                    end: None,
                });
            } else if let Some(period) =
                periods.iter_mut().rev().find(|period| period.end.is_none())
            {
                period.end = Some(today);
            }
        }
        self.modes.insert(name.clone(), mode);
        if mode != TrackingMode::OnDemand {
            self.forced_downloads_queued.remove(&file.name);
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
        self.archive_dirty = true;
        self.invalidate_sidebar_status();
        let selected = self.selected_file().map(LogFile::key);
        let tracked_keys: BTreeSet<_> = self
            .files
            .iter()
            .filter(|file| self.tracking_mode(file) != TrackingMode::Untracked)
            .map(LogFile::key)
            .collect();
        let tracked = |file: &LogFile| tracked_keys.contains(&file.key());
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
        self.empty_grid_port
            .is_none()
            .then(|| self.files.get(self.selected))
            .flatten()
    }

    pub fn selected_port(&self) -> Option<&str> {
        self.empty_grid_port
            .as_deref()
            .or_else(|| self.selected_file().and_then(Self::file_port))
    }

    pub(crate) fn empty_grid_cell(&self) -> bool {
        self.empty_grid_port.is_some()
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
        Some(snapshot.read_path(&self.cache_root))
    }

    pub fn cache_age(&self, file: &LogFile, now: DateTime<Utc>) -> String {
        self.snapshots
            .get(&file.key())
            .and_then(|snapshot| snapshot.updated_at.as_deref())
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| format_cache_age(now.signed_duration_since(timestamp).num_seconds()))
            .unwrap_or_else(|| "n/a".into())
    }

    /// Match cached copies to the server's current rotation names. A rotation can
    /// move from an unsuffixed name to _1 (and subsequently _2, etc.).
    fn grid_snapshots(&self) -> HashMap<String, &Snapshot> {
        let base = |name: &str| {
            let name = name.trim_end_matches(".txt");
            name.rsplit_once('_')
                .filter(|(_, suffix)| {
                    !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
                })
                .map_or(name, |(base, _)| base)
                .to_owned()
        };
        let date = |value: &str| {
            DateTime::parse_from_rfc3339(value)
                .ok()
                .map(|t| t.with_timezone(&self.server_offset).date_naive())
        };
        let mut candidates: HashMap<(String, String, NaiveDate), Vec<&LogFile>> = HashMap::new();
        for file in self.files.iter().filter(|file| file.present) {
            if let Some(day) = date(&file.modified) {
                candidates
                    .entry((file.host.clone(), base(&file.name), day))
                    .or_default()
                    .push(file);
            }
        }
        let files_by_key: HashMap<_, _> =
            self.files.iter().map(|file| (file.key(), file)).collect();
        let mut mapped = HashMap::new();
        for (key, snapshot) in &self.snapshots {
            if self.retained_archive_days.contains_key(key) {
                mapped.insert(key.clone(), snapshot);
                continue;
            }
            let Some(original) = files_by_key.get(key) else {
                continue;
            };
            let Some(day) = date(&snapshot.file.modified) else {
                mapped.insert(key.clone(), snapshot);
                continue;
            };
            let matches = candidates.get(&(original.host.clone(), base(&original.name), day));
            let target = matches.and_then(|files| {
                let eligible: Vec<_> = files
                    .iter()
                    .filter(|file| {
                        file.size >= snapshot.text_bytes
                            && DateTime::parse_from_rfc3339(&file.modified)
                                .ok()
                                .zip(DateTime::parse_from_rfc3339(&snapshot.file.modified).ok())
                                .is_some_and(|(remote, cached)| remote >= cached)
                    })
                    .collect();
                (eligible.len() == 1).then(|| eligible[0].key())
            });
            if let Some(target) = target {
                // Prefer the cache written directly for this rotation when both match.
                if target == *key || !mapped.contains_key(&target) {
                    mapped.insert(target, snapshot);
                }
            } else {
                // The server may have rotated or truncated this file without
                // exposing a matching rotation. Keep its local data readable.
                mapped.entry(key.clone()).or_insert(snapshot);
            }
        }
        mapped
    }

    pub(crate) fn grid_freshness(&self, now: DateTime<Utc>) -> BTreeMap<&str, CacheFreshness> {
        let snapshots = self.grid_snapshots();
        let mut groups: BTreeMap<&str, (bool, bool, bool, bool)> = BTreeMap::new();
        for file in &self.files {
            let snapshot = snapshots.get(&file.key()).copied();
            if !file.present
                && snapshot.is_none()
                && self.tracking_mode(file) == TrackingMode::Untracked
            {
                continue;
            }
            let status = match snapshot {
                Some(snapshot)
                    if snapshot.text_bytes > 0
                        && snapshot.settled(self.server_offset)
                        && (!file.present
                            || snapshot.text_bytes == file.size
                                && history::same_day(&snapshot.file, file, self.server_offset)) =>
                {
                    CacheFreshness::Fresh
                }
                None if !file.present || file.size == 0 => CacheFreshness::NoData,
                None => CacheFreshness::Missing,
                Some(snapshot) if file.size == 0 && snapshot.text_bytes == 0 => {
                    CacheFreshness::NoData
                }
                Some(_) if file.size == 0 => CacheFreshness::Stale,
                Some(snapshot) if file.size > 0 && snapshot.text_bytes == 0 => {
                    CacheFreshness::Missing
                }
                Some(snapshot) if Self::file_days_ago(file).is_some_and(|day| day > 0) => {
                    if (!file.present && self.inventoried_hosts.contains(&file.host))
                        || (snapshot.full_size == Some(file.size)
                            && snapshot.text_bytes == file.size
                            && (snapshot.file.key() == file.key()
                                || Self::same_modified(&snapshot.file.modified, &file.modified)))
                    {
                        CacheFreshness::Fresh
                    } else {
                        CacheFreshness::Stale
                    }
                }
                Some(snapshot)
                    if snapshot
                        .updated_at
                        .as_deref()
                        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
                        .is_some_and(|time| {
                            now.signed_duration_since(time.with_timezone(&Utc))
                                .num_seconds()
                                < 30 * 60
                        }) =>
                {
                    CacheFreshness::Fresh
                }
                Some(_) => CacheFreshness::Stale,
            };
            let entry = groups.entry(file.name.as_str()).or_default();
            match status {
                CacheFreshness::Fresh => entry.0 = true,
                CacheFreshness::Stale => entry.1 = true,
                CacheFreshness::Missing => entry.2 = true,
                CacheFreshness::NoData => entry.3 = true,
            }
        }
        groups
            .into_iter()
            .map(|(name, (fresh, stale, missing, no_data))| {
                (
                    name,
                    if !fresh && !stale && !missing && no_data {
                        CacheFreshness::NoData
                    } else if !fresh && !stale {
                        CacheFreshness::Missing
                    } else if stale || missing {
                        CacheFreshness::Stale
                    } else {
                        CacheFreshness::Fresh
                    },
                )
            })
            .collect()
    }

    pub(crate) fn grid_tracking_modes(&self) -> BTreeMap<&str, TrackingMode> {
        let mut result = BTreeMap::new();
        for file in &self.files {
            let mode = self.tracking_mode(file);
            result
                .entry(file.name.as_str())
                .and_modify(|current| {
                    if *current == TrackingMode::Untracked {
                        *current = mode;
                    }
                })
                .or_insert(mode);
        }
        result
    }

    pub fn cache_age_seconds(&self, file: &LogFile, now: DateTime<Utc>) -> Option<i64> {
        self.snapshots
            .get(&file.key())
            .and_then(|snapshot| snapshot.updated_at.as_deref())
            .and_then(|timestamp| DateTime::parse_from_rfc3339(timestamp).ok())
            .map(|timestamp| now.signed_duration_since(timestamp).num_seconds().max(0))
    }

    pub fn cache_group_age_seconds(&self, file: &LogFile, now: DateTime<Utc>) -> Option<i64> {
        let copies: Vec<_> = self
            .files
            .iter()
            .filter(|copy| {
                copy.name == file.name && Self::file_days_ago(copy) == Self::file_days_ago(file)
            })
            .collect();
        if copies.is_empty() {
            return None;
        }
        copies
            .iter()
            .map(|copy| self.cache_age_seconds(copy, now))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max()
    }

    pub fn merged_cache_age_seconds(&self, port: &str, now: DateTime<Utc>) -> Option<i64> {
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|file| {
                Self::file_port(file) == Some(port)
                    && Self::file_days_ago(file) == Some(self.discovery_days_ago)
                    && matches!(
                        presentation::source(&file.name).0,
                        Kind::Error | Kind::Access
                    )
            })
            .collect();
        if files.is_empty() {
            return None;
        }
        files
            .iter()
            .map(|file| self.cache_age_seconds(file, now))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .max()
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
        if self.has_log_selection() {
            self.selected_log_names.clear();
            self.combined = false;
        }
        if !self.combined {
            self.all_cached = false;
        }
        let previous_name = self.selected_file().map(|file| file.name.clone());
        let previous_key = self.selected_file().map(LogFile::key);
        self.empty_grid_port = None;
        self.selected = index.min(self.files.len().saturating_sub(1));
        if self.selected_file().map(LogFile::key) != previous_key {
            if let Some(page) = &mut self.page_navigation {
                page.pending = None;
            }
        }
        if let Some(file) = self.selected_file() {
            self.sidebar_kind = usize::from(Self::log_kind_sort_key(file).min(2));
        }
        if self.selected_file().map(|file| file.name.as_str()) != previous_name.as_deref() {
            self.host_detail_offset = 0;
        }
        self.horizontal = 0;
        self.host_anchor_position = None;
        if self.preview_pending() {
            return;
        }
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

    pub(crate) fn file_days_ago(file: &LogFile) -> Option<u32> {
        Self::name_days_ago(&file.name)
    }

    fn name_days_ago(name: &str) -> Option<u32> {
        let name = name.strip_suffix(".txt")?;
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
        let name = file.name.trim_end_matches(".txt");
        let name = name
            .rsplit_once('_')
            .filter(|(_, suffix)| {
                !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or(name, |(base, _)| base);
        if name.starts_with("TaskServer_") {
            return Some("TASK");
        }
        let Some((port, _)) = name.split_once('_') else {
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

    pub(crate) fn host_names(&self) -> Vec<String> {
        self.known_hosts().into_iter().map(str::to_owned).collect()
    }

    pub(crate) fn host_enabled(&self, host: &str) -> bool {
        !self.excluded_hosts.contains(host)
    }

    pub(crate) fn host_filter_active(&self) -> bool {
        !self.excluded_hosts.is_empty()
    }

    fn apply_host_filter(&mut self) {
        let cursor = self.ensure_cursor_time();
        if let Some(selected) = self.selected_file()
            && !self.host_enabled(&selected.host)
        {
            let name = selected.name.clone();
            if let Some(index) = self
                .files
                .iter()
                .position(|file| {
                    file.name == name
                        && self.host_enabled(&file.host)
                        && self.snapshots.contains_key(&file.key())
                })
                .or_else(|| {
                    self.files
                        .iter()
                        .position(|file| file.name == name && self.host_enabled(&file.host))
                })
            {
                self.selected = index;
            }
        }
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = cursor {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
    }

    pub(crate) fn toggle_host(&mut self, index: usize) {
        let Some(host) = self.host_names().get(index).cloned() else {
            return;
        };
        self.host_selected = index;
        if !self.excluded_hosts.remove(&host) {
            self.excluded_hosts.insert(host.clone());
        }
        self.apply_host_filter();
        self.message = format!(
            "{host}: {} in log view.",
            if self.host_enabled(&host) {
                "included"
            } else {
                "excluded"
            }
        );
    }

    pub(crate) fn only_host(&mut self, index: usize) {
        let hosts = self.host_names();
        let Some(host) = hosts.get(index) else {
            return;
        };
        self.excluded_hosts = hosts
            .iter()
            .filter(|candidate| *candidate != host)
            .cloned()
            .collect();
        self.apply_host_filter();
        self.message = format!("Showing logs from {host} only.");
    }

    pub(crate) fn enable_all_hosts(&mut self) {
        self.excluded_hosts.clear();
        self.apply_host_filter();
        self.message = "Showing logs from all hosts.".into();
    }

    pub(crate) fn move_host_selection(&mut self, forwards: bool) {
        let count = self.host_names().len();
        if count == 0 {
            return;
        }
        self.host_selected = if forwards {
            self.host_selected.saturating_add(1).min(count - 1)
        } else {
            self.host_selected.saturating_sub(1)
        };
        if self.host_selected < self.host_detail_offset {
            self.host_detail_offset = self.host_selected;
        } else if self.host_detail_capacity > 0
            && self.host_selected >= self.host_detail_offset + self.host_detail_capacity
        {
            self.host_detail_offset = self.host_selected + 1 - self.host_detail_capacity;
        }
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

    /// Calendar selection narrows the retained truncation bounds; an explicit
    /// row selection and the all-cached view can still span multiple days.
    fn display_window(&self) -> TimeWindow {
        if (!self.combined && self.all_cached)
            || (self.has_log_selection() && self.selection_across_days)
        {
            return self.window;
        }
        let day = self.discovery_day();
        let start = day.and_hms_opt(0, 0, 0).unwrap();
        let end = day.and_hms_nano_opt(23, 59, 59, 999_999_999).unwrap();
        if self.automatic_window && self.discovery_days_ago > 0 {
            return TimeWindow {
                start: Some(start),
                end: Some(end),
            };
        }
        TimeWindow {
            start: Some(self.window.start.map_or(start, |bound| bound.max(start))),
            end: Some(self.window.end.map_or(end, |bound| bound.min(end))),
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
        self.empty_grid_port = None;
        self.discovery_days_ago = days;
        self.discovery_revision = self.discovery_revision.wrapping_add(1);
        self.ensure_visible_selection();
        self.rebuild_lines();
        self.refresh(rt);
    }

    fn prune_untracked_inventory(&mut self) {
        let selected = self.selected_file().map(LogFile::key);
        let tracked: BTreeSet<_> = self
            .files
            .iter()
            .filter(|file| self.tracking_mode(file) != TrackingMode::Untracked)
            .map(LogFile::key)
            .collect();
        let snapshots = &self.snapshots;
        self.files
            .retain(|file| tracked.contains(&file.key()) || snapshots.contains_key(&file.key()));
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
            .filter(|_| self.is_grid_file_visible(selected))
    }

    /// A sidebar row represents a port. Each cell points to the visible file
    /// group for that port and log kind (error, access, request).
    pub fn visible_port_rows(&self) -> Vec<(String, [Option<usize>; 3])> {
        self.port_rows_for_day(self.discovery_days_ago)
    }

    pub(crate) fn port_rows_for_day(&self, day: u32) -> Vec<(String, [Option<usize>; 3])> {
        self.cached_port_rows(day)
    }

    fn build_port_rows_for_day(&self, day: u32) -> Vec<(String, [Option<usize>; 3])> {
        let mut rows: BTreeMap<String, [Option<usize>; 3]> = BTreeMap::new();
        let aliases = (!self.file_filter.trim().is_empty()).then(|| self.host_aliases());
        for (index, file) in self.files.iter().enumerate() {
            if self.retained_archive_days.contains_key(&file.key())
                || Self::file_days_ago(file) != Some(day)
                || !self.is_grid_file_visible_with_aliases(file, aliases.as_ref())
            {
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
        // Cached files can span calendar days independently of their rotation number.
        // Surface those days once the stream has been parsed, without reading files
        // from disk during rendering.
        let date = self.now().date() - Days::days(i64::from(day));
        for (index, file) in self.files.iter().enumerate() {
            if self
                .retained_archive_days
                .get(&file.key())
                .is_some_and(|days| days.contains(&date))
                && self.is_grid_file_visible_with_aliases(file, aliases.as_ref())
                && let Some(port) = Self::file_port(file)
            {
                let kind = usize::from(Self::log_kind_sort_key(file));
                if kind < 3 {
                    rows.entry(port.to_owned()).or_default()[kind] = Some(index);
                }
            }
        }
        for (index, file) in self.files.iter().enumerate() {
            let Some(stream) = self.parsed_streams.get(&file.key()) else {
                continue;
            };
            let start = date.and_hms_opt(0, 0, 0).unwrap();
            let end = date.and_hms_nano_opt(23, 59, 59, 999_999_999).unwrap();
            if !self.is_grid_file_visible_with_aliases(file, aliases.as_ref())
                || !stream.records[stream.window_range(Some(start), Some(end))]
                    .iter()
                    .any(|record| record.timestamp.is_some_and(|time| time.date() == date))
            {
                continue;
            }
            let Some(port) = Self::file_port(file) else {
                continue;
            };
            let kind = usize::from(Self::log_kind_sort_key(file));
            if kind < 3 {
                let cell = &mut rows.entry(port.to_owned()).or_default()[kind];
                if cell.is_none() {
                    *cell = Some(index);
                }
            }
        }
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_by(|(left, _), (right, _)| {
            Self::port_label_sort_key(left).cmp(&Self::port_label_sort_key(right))
        });
        rows
    }

    fn is_grid_file_visible(&self, file: &LogFile) -> bool {
        self.is_grid_file_visible_with_aliases(file, None)
    }

    fn is_grid_file_visible_with_aliases(
        &self,
        file: &LogFile,
        aliases: Option<&BTreeMap<String, String>>,
    ) -> bool {
        let port_filter = Self::file_port(file).and_then(|port| self.port_filters.get(port));
        Self::file_port(file).is_some()
            && (self.tracking_mode(file) != TrackingMode::Untracked
                || file.present
                || self.snapshots.contains_key(&file.key()))
            && !matches!(port_filter, Some(PortFilter::Exclude))
            && self.matches_file_filter(file, aliases)
            && (!self.include_only_marked_ports || matches!(port_filter, Some(PortFilter::Include)))
    }

    pub(crate) fn has_log_selection(&self) -> bool {
        !self.selected_log_names.is_empty()
    }

    pub(crate) fn is_log_selected(&self, file: &LogFile) -> bool {
        self.selected_log_names.contains(&file.name)
    }

    pub(crate) fn log_selection_count(&self) -> usize {
        self.selected_log_names.len()
    }

    pub(crate) fn log_selection_scope_label(&self) -> String {
        if self.selection_across_days {
            "all days".into()
        } else {
            self.discovery_day_label()
        }
    }

    pub(crate) fn merged_port_view(&self) -> bool {
        self.combined && !self.has_log_selection()
    }

    /// Select this port/type across days, or this day/type across visible ports.
    /// Keep filenames rather than inventory positions so refreshes and sorting
    /// cannot silently change the search scope.
    pub(crate) fn select_log_group(&mut self, across_days: bool) {
        if self.selected_visible_index().is_none() {
            self.message = "No matching file selected. Clear filters with c or P.".into();
            return;
        }
        let Some(selected) = self.selected_file() else {
            return;
        };
        let kind = Self::log_kind_sort_key(selected);
        let names = self
            .files
            .iter()
            .filter(|file| {
                Self::log_kind_sort_key(file) == kind
                    && self.is_grid_file_visible(file)
                    && if across_days {
                        Self::file_port(file) == Self::file_port(selected)
                            && Self::file_days_ago(file).is_some()
                    } else {
                        Self::file_days_ago(file) == Some(self.discovery_days_ago)
                    }
            })
            .map(|file| file.name.clone())
            .collect();
        let cursor = self.ensure_cursor_time();
        self.selected_log_names = names;
        self.selection_across_days = across_days;
        self.combined = true;
        self.all_cached = false;
        self.host_anchor_position = None;
        self.rebuild_lines();
        if let Some(time) = cursor {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
        self.message = format!(
            "Selected {} logs across {} (all hosts). Enter, then / to search; Esc or move to clear.",
            self.log_selection_count(),
            if across_days { "days" } else { "ports" },
        );
    }

    pub(crate) fn clear_log_selection(&mut self) {
        if self.has_log_selection() {
            self.select(self.selected);
            self.message = "Log selection cleared.".into();
        }
    }

    pub fn selected_kind_column(&self) -> usize {
        self.sidebar_kind.min(2)
    }

    pub fn selected_display_column(&self) -> usize {
        if self.merged_port_view() {
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

    pub(crate) fn port_label_sort_key(port: &str) -> (u8, u16, &str) {
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
        } else if self.merged_port_view()
            && preferred_kind < 2
            && cells[1 - preferred_kind].is_some()
        {
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
        let merged = self.merged_port_view();
        let count = if merged { 2 } else { 3 };
        let current = self.selected_display_column() as isize;
        for step in 1..count {
            let column = (current + direction * step).rem_euclid(count) as usize;
            let candidate = |cells: &[Option<usize>; 3]| {
                if merged && column == 0 {
                    cells[0]
                        .filter(|&index| eligible(index))
                        .or_else(|| cells[1].filter(|&index| eligible(index)))
                } else {
                    cells[if merged { 2 } else { column }].filter(|&index| eligible(index))
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

    pub(crate) fn grid_ports(&self, first_day: u32, count: usize) -> Vec<String> {
        let mut ports: Vec<String> = (0..count)
            .flat_map(|n| {
                self.port_rows_for_day(first_day.saturating_add(n as u32))
                    .into_iter()
                    .map(|(port, _)| port)
            })
            .collect();
        if let Some(port) = self.empty_grid_port.as_ref()
            && self.discovery_days_ago >= first_day
            && u64::from(self.discovery_days_ago) < u64::from(first_day) + count as u64
        {
            ports.push(port.clone());
        }
        ports.sort_by(|a, b| Self::port_label_sort_key(a).cmp(&Self::port_label_sort_key(b)));
        ports.dedup();
        ports
    }

    pub(crate) fn select_grid_cell(&mut self, port: &str, day: u32, kind: usize) {
        let index = self
            .port_rows_for_day(day)
            .iter()
            .find(|(p, _)| p == port)
            .and_then(|(_, cells)| cells.get(kind).copied().flatten());
        if let Some(index) = index {
            self.select_grid_index(day, index);
        } else if let Some(index) = self.files.iter().position(|file| {
            Self::file_port(file) == Some(port)
                && usize::from(Self::log_kind_sort_key(file)) == kind
                && self
                    .snapshots
                    .get(&file.key())
                    .is_some_and(|snapshot| snapshot.text_bytes > 0 || !snapshot.text.is_empty())
        }) {
            self.select_grid_index(day, index);
        } else {
            self.ensure_cursor_time();
            self.empty_grid_port = Some(port.to_owned());
            self.discovery_days_ago = day;
            self.sidebar_kind = kind.min(2);
            self.day_selection = None;
            if self.has_log_selection() {
                self.selected_log_names.clear();
                self.combined = false;
            }
            self.all_cached = false;
            self.host_anchor_position = None;
            self.rebuild_lines();
            self.message = "No log is available in this cell.".into();
        }
    }

    fn select_grid_index(&mut self, day: u32, index: usize) {
        self.discovery_days_ago = day;
        self.day_selection = None;
        self.select(index);
    }

    pub(crate) fn move_grid_horizontal_any(&mut self, forwards: bool) {
        let Some(port) = self.selected_port().map(str::to_owned) else {
            return;
        };
        let position = u64::from(self.discovery_days_ago) * 3 + self.selected_kind_column() as u64;
        let maximum_day = self
            .files
            .iter()
            .filter_map(Self::file_days_ago)
            .max()
            .unwrap_or(0)
            .max(
                self.grid_first_day
                    .saturating_add(self.grid_day_count.saturating_sub(1) as u32),
            )
            .min(36_500);
        let next = if forwards {
            (position + 1).min(u64::from(maximum_day) * 3 + 2)
        } else {
            position.saturating_sub(1)
        };
        if next != position {
            self.select_grid_cell(&port, (next / 3) as u32, (next % 3) as usize);
        }
    }

    pub(crate) fn move_grid_vertical_any(&mut self, forwards: bool) {
        let Some(port) = self.selected_port() else {
            return;
        };
        let ports = self.grid_ports(self.grid_first_day, self.grid_day_count);
        let Some(index) = ports.iter().position(|candidate| candidate == port) else {
            return;
        };
        let next = if forwards {
            (index + 1).min(ports.len().saturating_sub(1))
        } else {
            index.saturating_sub(1)
        };
        if next != index {
            self.select_grid_cell(
                &ports[next],
                self.discovery_days_ago,
                self.selected_kind_column(),
            );
        }
    }

    pub fn select_grid_horizontal(&mut self, forwards: bool, minor: bool) {
        let Some(port) = self.selected_port().map(str::to_owned) else {
            return;
        };
        let adjacent_day = if forwards {
            self.discovery_days_ago
                .checked_add(1)
                .filter(|day| *day <= 36_500)
        } else {
            self.discovery_days_ago.checked_sub(1)
        };
        if let Some(day) = adjacent_day
            && let Some(index) = self
                .port_rows_for_day(day)
                .iter()
                .find(|(candidate, _)| candidate == &port)
                .and_then(|(_, cells)| cells[self.selected_kind_column()])
            && Self::file_days_ago(&self.files[index]) != Some(day)
        {
            self.select_grid_index(day, index);
            return;
        }
        let current = i64::from(self.discovery_days_ago) * 3 + self.selected_kind_column() as i64;
        let current_key = Self::port_label_sort_key(&port);
        let target = self
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let day = Self::file_days_ago(file)?;
                let candidate_port = Self::file_port(file)?;
                let kind = Self::log_kind_sort_key(file);
                if kind > 2 {
                    return None;
                }
                let position = i64::from(day) * 3 + i64::from(kind);
                if (forwards && position <= current) || (!forwards && position >= current) {
                    return None;
                }
                let tracked = self.tracking_mode(file) != TrackingMode::Untracked;
                let cached = self.snapshots.contains_key(&file.key());
                let same_stream =
                    candidate_port == port && usize::from(kind) == self.selected_kind_column();
                let port_filter = self.port_filters.get(candidate_port);
                if (!tracked
                    && !cached
                    && !(same_stream && file.present)
                    && !(minor && file.present))
                    || matches!(port_filter, Some(PortFilter::Exclude))
                    || !self.matches_file_filter(file, None)
                    || (self.include_only_marked_ports
                        && !matches!(port_filter, Some(PortFilter::Include)))
                {
                    return None;
                }
                let key = Self::port_label_sort_key(candidate_port);
                Some((
                    (
                        usize::from(candidate_port != port),
                        position.abs_diff(current),
                        key.0.abs_diff(current_key.0),
                        key.1.abs_diff(current_key.1),
                    ),
                    day,
                    index,
                ))
            })
            .min_by_key(|(distance, _, _)| *distance);
        let same_stream_target = target.as_ref().is_some_and(|(_, _, index)| {
            Self::file_port(&self.files[*index]) == Some(&port)
                && usize::from(Self::log_kind_sort_key(&self.files[*index]))
                    == self.selected_kind_column()
        });
        if !same_stream_target
            && let Some(day) = adjacent_day
            && self.files.iter().any(|file| {
                Self::file_port(file) == Some(&port)
                    && usize::from(Self::log_kind_sort_key(file)) == self.selected_kind_column()
                    && self
                        .snapshots
                        .get(&file.key())
                        .is_some_and(|snapshot| snapshot.text_bytes > 0)
            })
        {
            self.select_grid_cell(&port, day, self.selected_kind_column());
        } else if let Some((_, day, index)) = target {
            self.select_grid_index(day, index);
        }
    }

    fn ensure_visible_selection(&mut self) {
        if let Some(port) = self.empty_grid_port.as_deref() {
            if self
                .files
                .iter()
                .any(|file| Self::file_port(file) == Some(port) && self.is_grid_file_visible(file))
            {
                return;
            }
            self.empty_grid_port = None;
        }
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
            if self.merged_port_view() && column < 2 {
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
        if self.automatic_window
            && self.discovery_days_ago > 0
            && !self.selection_across_days
            && !self.all_cached
        {
            return None;
        }
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

    fn clear_empty_grid_view(&mut self) {
        self.lines.clear();
        self.rows.clear();
        self.row_sources.clear();
        self.rendered_records.borrow_mut().clear();
        self.host_views.clear();
        self.matches.clear();
        self.time_index.clear();
        self.parse_jobs.clear();
        self.activity_graph.set_data(Vec::new());
        self.scroll = 0;
    }

    fn rebuild_lines(&mut self) {
        if self.preview_pending() {
            return;
        }
        self.rendered_day = Some(self.discovery_day());
        let window = self.display_window();
        let include_untimed = self.window.contains(None);
        if self.empty_grid_cell() {
            self.clear_empty_grid_view();
            return;
        }
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
                .filter(|file| self.is_file_visible(file) && self.host_enabled(&file.host))
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
                    if !window.contains(time) {
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
        } else if let Some(selected) = self.selected_file().cloned() {
            // A rotation can contain records from more than one calendar day.
            // Read cached rotations of the same stream and filter by timestamp.
            let mut files: Vec<_> = self
                .files
                .iter()
                .filter(|file| {
                    Self::file_port(file) == Self::file_port(&selected)
                        && Self::log_kind_sort_key(file) == Self::log_kind_sort_key(&selected)
                        && self.host_enabled(&file.host)
                })
                .cloned()
                .collect();
            files.sort_by(|left, right| left.host.cmp(&right.host));
            let mapped = self.grid_snapshots();
            let files: Vec<_> = files
                .into_iter()
                .map(|file| {
                    let key = mapped
                        .get(&file.key())
                        .map(|snapshot| snapshot.file.key())
                        .unwrap_or_else(|| file.key());
                    (file, key)
                })
                .collect();
            let mut seen = BTreeSet::new();
            let files: Vec<_> = files
                .into_iter()
                .filter(|(_, key)| {
                    seen.insert(
                        self.snapshots
                            .get(key)
                            .map(Snapshot::source_identity)
                            .unwrap_or_else(|| key.clone()),
                    )
                })
                .collect();
            let wanted: BTreeSet<_> = files.iter().map(|(_, key)| key.clone()).collect();
            self.parse_jobs.retain(|key, _| wanted.contains(key));
            // A host can contribute several physical rotations to this window.
            // Collect record references first so panes represent hosts, not files.
            let mut by_host: BTreeMap<String, Vec<(Option<NaiveDateTime>, Arc<str>, usize)>> =
                BTreeMap::new();
            for (file, key) in files {
                if !self.snapshots.get(&key).is_some_and(|snapshot| {
                    snapshot.text_bytes > 0 || !snapshot.text.trim().is_empty()
                }) || !self.prepare_stream(&key)
                {
                    continue;
                }
                let Some(stream) = self.parsed_streams.get(&key) else {
                    continue;
                };
                let shared_key: Arc<str> = Arc::from(key.as_str());
                let range = stream.window_range(window.start, window.end);
                let host_records = by_host.entry(file.host).or_default();
                for (record_index, record) in stream
                    .records
                    .iter()
                    .enumerate()
                    .skip(range.start)
                    .take(range.len())
                {
                    if window.contains(record.timestamp)
                        || (include_untimed && record.timestamp.is_none())
                    {
                        host_records.push((record.timestamp, shared_key.clone(), record_index));
                    }
                }
            }
            for (host, mut records) in by_host {
                if records.is_empty() {
                    continue;
                }
                let keys: Vec<_> = records
                    .iter()
                    .map(|(_, key, _)| key.as_ref())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                // Keep multiline records together, with stable ordering at equal times.
                if keys.len() > 1 {
                    records.sort_unstable();
                }
                let first = records.iter().find_map(|(time, _, _)| *time);
                let mut rows = Vec::new();
                let mut row_sources = Vec::new();
                if let Some(mut marker) = self.truncation_marker(true) {
                    marker.line = Line::from(format!("[{host}] {}", marker.line));
                    rows.push(marker);
                    row_sources.push(None);
                }
                for (_, key, record_index) in records {
                    let record = &self.parsed_streams[key.as_ref()].records[record_index];
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
                            key: key.clone(),
                            record: record_index,
                            line,
                            first,
                            raw_range: None,
                            wrapped: false,
                        }));
                    }
                }
                if let Some(mut marker) = self.truncation_marker(false) {
                    marker.line = Line::from(format!("[{host}] {}", marker.line));
                    rows.push(marker);
                    row_sources.push(None);
                }
                self.host_views.push(HostView {
                    host,
                    keys,
                    rows,
                    matches: Vec::new(),
                    row_sources,
                    time_index: Vec::new(),
                });
            }
            let selected_host = self.selected_file().map(|file| file.host.clone());
            let lead = self
                .host_views
                .iter()
                .position(|view| Some(&view.host) == selected_host.as_ref())
                .unwrap_or(0);
            if let Some(view) = self.host_views.get(lead) {
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
        self.search_changed_at = None;
        self.schedule_index_filter();
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
        let leading_host = self
            .leading_host_index()
            .map(|index| self.host_views[index].host.clone());
        for view in &mut self.host_views {
            let mut visible = vec![false; view.rows.len()];
            let mut eligible = vec![true; view.rows.len()];
            let first = view
                .row_sources
                .iter()
                .find_map(|source| source.as_ref().and_then(|source| source.first));
            let mut last_record = None;
            let mut rendered = Vec::new();
            let mut raw_match = false;
            // Context remains local to each physical file even when rotations
            // interleave chronologically in the same host pane.
            let mut context_rows: HashMap<&str, Vec<usize>> = HashMap::new();
            let mut context_offsets = Vec::new();
            if self.context_before > 0 || self.context_after > 0 {
                context_offsets.resize(view.rows.len(), 0);
                for (index, source) in view.row_sources.iter().enumerate() {
                    if let Some(source) = source {
                        context_rows
                            .entry(source.key.as_ref())
                            .or_default()
                            .push(index);
                    }
                }
                for indices in context_rows.values_mut() {
                    indices.sort_unstable_by_key(|&index| {
                        let source = view.row_sources[index].as_ref().unwrap();
                        (source.record, source.line)
                    });
                    for (offset, &index) in indices.iter().enumerate() {
                        context_offsets[index] = offset;
                    }
                }
            }
            for (index, source) in view.row_sources.iter().enumerate() {
                let Some(source) = source else {
                    visible[index] = true;
                    continue;
                };
                let stream = self.parsed_streams.get(source.key.as_ref());
                let use_indexed_matches = self.index_filter_changed.is_none()
                    && self.index_filter_active == self.index_filter_target
                    && self
                        .parsed_large
                        .get(source.key.as_ref())
                        .is_some_and(|(spec, _)| {
                            spec.filter_id != 0 && spec.filter_id == self.index_filter_revision
                        });
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
                    if last_record != Some((source.key.as_ref(), source.record)) {
                        raw_match = record.raw.to_lowercase().contains(&needle);
                        rendered = if raw_match
                            || (use_indexed_matches && record.indexed_search_rows.is_some())
                        {
                            Vec::new()
                        } else {
                            record.render(&self.options, first, None, false)
                        };
                        last_record = Some((source.key.as_ref(), source.record));
                    }
                    raw_match
                        || (use_indexed_matches
                            && record
                                .indexed_search_rows
                                .as_ref()
                                .and_then(|rows| rows.get(source.line))
                                .copied()
                                .unwrap_or(false))
                        || rendered
                            .get(source.line)
                            .is_some_and(|line| line.to_string().to_lowercase().contains(&needle))
                } else {
                    false
                };
                if matched {
                    visible[index] = true;
                    if let Some(indices) = context_rows.get(source.key.as_ref()) {
                        let offset = context_offsets[index];
                        let start = offset.saturating_sub(self.context_before);
                        let end = offset
                            .saturating_add(self.context_after)
                            .min(indices.len().saturating_sub(1));
                        for &context in &indices[start..=end] {
                            visible[context] = true;
                        }
                    }
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
        if !self.host_views.is_empty() {
            self.refresh_activity_graph();
        }
        if let Some(view) = self
            .host_views
            .iter()
            .find(|view| Some(&view.host) == leading_host.as_ref())
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
        self.refresh_activity_graph();
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

    fn refresh_activity_graph(&mut self) {
        let mut times = Vec::new();
        let mut collect = |rows: &[ViewRow], matches: &[usize], sources: &[Option<RowSource>]| {
            let mut scanned = 0;
            let mut record = None;
            let mut counted = None;
            for &index in matches {
                let Some(row) = rows.get(index) else {
                    continue;
                };
                // Find the logical record even when a filter only retains one
                // of its continuation lines. Matches are in source row order.
                for (offset, row) in rows[scanned..=index].iter().enumerate() {
                    if row.start {
                        record = Some(scanned + offset);
                    }
                }
                scanned = index + 1;
                if !row.truncation_marker
                    && (row.start || sources.get(index).is_some_and(Option::is_some))
                    && record.is_some()
                    && record != counted
                    && let Some(time) = row.timestamp
                {
                    times.push(time);
                    counted = record;
                }
            }
        };
        if self.host_views.is_empty() {
            collect(&self.rows, &self.matches, &self.row_sources);
        } else {
            for view in &self.host_views {
                collect(&view.rows, &view.matches, &view.row_sources);
            }
        }
        self.activity_graph.set_data(times);
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
        let host = self.selected_file().map(|file| &file.host);
        self.host_views
            .iter()
            .position(|view| Some(&view.host) == host)
            .or_else(|| (!self.host_views.is_empty()).then_some(0))
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
        let host = self.host_views[next].host.clone();
        let selected_name = self.selected_file().map(|file| file.name.clone());
        if let Some(index) = self
            .files
            .iter()
            .position(|file| file.host == host && Some(&file.name) == selected_name.as_ref())
        {
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
        if self.cache_loading() {
            self.cancel();
        }
        self.truncation_sequence = None;
        self.scope_days = 1;
        self.window = TimeWindow {
            start: Some(self.now() - Days::minutes(10)),
            end: None,
        };
        self.window_initialized = true;
        self.default_truncation = self.clock.is_none();
        self.automatic_window = true;
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
        if self.files_focus && !backwards {
            self.flush_file_preview();
        }
        let split = !self.combined && !self.all_cached && self.host_views.len() > 1;
        if !self.sidebar_visible {
            self.hosts_focus = false;
            self.files_focus = false;
            if split {
                self.cycle_leading_host(backwards);
            }
            return;
        }
        if self.hosts_focus {
            self.hosts_focus = false;
            self.files_focus = !backwards;
            if backwards && split {
                self.select_leading_host(self.host_views.len() - 1);
            }
            return;
        }
        if self.files_focus {
            if backwards {
                self.hosts_focus = true;
            } else {
                self.files_focus = false;
                if split {
                    self.select_leading_host(0);
                }
            }
            return;
        }
        let current = self.leading_host_index().unwrap_or(0);
        if split
            && ((!backwards && current + 1 < self.host_views.len()) || (backwards && current > 0))
        {
            self.cycle_leading_host(backwards);
        } else if backwards {
            self.files_focus = true;
        } else {
            self.files_focus = true;
            self.hosts_focus = true;
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
        if ((delta < 0 && current <= starts[0])
            || (delta > 0 && current >= *starts.last().unwrap()))
            && self.page_large_source(delta < 0)
        {
            return true;
        }
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
        self.clear_cached_logs_without_sync();
        self.sync(rt, None);
    }

    pub fn begin_storage_operation(&mut self) {
        self.cancel();
        self.storage_busy = true;
    }

    pub fn finish_storage_operation(&mut self) {
        self.invalidate_sidebar_status();
        self.storage_busy = false;
        self.snapshots.clear();
        self.parsed_streams.clear();
        self.parsed_large.clear();
        self.read_failures.clear();
        self.archived_raw_cleanup.clear();
        self.parse_jobs.clear();
        self.page_navigation = None;
        if !self.cache_root.as_os_str().is_empty() {
            if let Err(error) = self.load_cache() {
                self.message = format!("Reloading managed cache: {error:#}");
            }
        }
        self.last_poll = Instant::now();
    }

    pub fn clear_cached_logs_without_sync(&mut self) {
        self.invalidate_sidebar_status();
        if self.cache_root.as_os_str().is_empty() {
            self.message = "Open Logs for an environment first; no cache was deleted.".into();
            return;
        }
        self.cancel();
        if let Err(error) = fs::create_dir_all(&self.cache_root) {
            self.message = format!("Could not clear cache: {error}");
            return;
        }
        let _storage_lock = match ArchivePublishLock::acquire(&self.cache_root) {
            Ok(lock) => lock,
            Err(error) => {
                self.message = format!("Could not clear cache: {error:#}");
                return;
            }
        };
        self.save_subscriptions();
        let mut failures = Vec::new();
        match fs::read_dir(&self.cache_root) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            failures.push(error.to_string());
                            continue;
                        }
                    };
                    let path = entry.path();
                    if matches!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some(
                            SUBSCRIPTIONS_FILE
                                | CATALOG_FILE
                                | filter::PRESETS_FILE
                                | ".storage.lock"
                        )
                    ) {
                        continue;
                    }
                    let result = if matches!(
                        path.file_name().and_then(|name| name.to_str()),
                        Some("archives" | ".staging" | "storage")
                    ) && path.is_dir()
                    {
                        fs::remove_dir_all(&path)
                    } else {
                        fs::remove_file(&path)
                    };
                    if let Err(error) = result {
                        if error.kind() != std::io::ErrorKind::NotFound {
                            failures.push(format!("{}: {error}", path.display()));
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failures.push(format!("{}: {error}", self.cache_root.display())),
        }
        self.snapshots.clear();
        self.history_state = history::HistoryState::default();
        self.history_checked = None;
        self.history_retry = None;
        self.archive_dirty = true;
        self.files
            .retain(|file| !self.retained_archive_days.contains_key(&file.key()));
        self.retained_archive_days.clear();
        self.parsed_streams.clear();
        self.parsed_large.clear();
        self.read_failures.clear();
        self.archived_raw_cleanup.clear();
        self.row_sources.clear();
        self.rendered_records.borrow_mut().clear();
        self.host_views.clear();
        self.rows.clear();
        self.lines.clear();
        self.matches.clear();
        self.all_cached = false;
        self.activity_graph.set_data(Vec::new());
        self.scroll = 0;
        self.message = if failures.is_empty() {
            "Cleared cached logs; tracking and catalog retained.".into()
        } else {
            format!(
                "Some cache files could not be cleared: {}",
                failures.join(" | ")
            )
        };
    }

    pub fn toggle_combined(&mut self) {
        let cursor = self.ensure_cursor_time();
        self.selected_log_names.clear();
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
        self.hosts_focus = false;
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
        if self.preview_pending() {
            return;
        }
        self.rendered_day = Some(self.discovery_day());
        let window = self.display_window();
        self.combined_dirty = false;
        if self.empty_grid_cell() {
            self.clear_empty_grid_view();
            return;
        }
        self.host_views.clear();
        let selected_port = self.selected_port().map(str::to_owned);
        let mut seen_sources = BTreeSet::new();
        let files: Vec<_> = self
            .files
            .iter()
            .filter(|file| {
                self.host_enabled(&file.host)
                    && self.snapshots.contains_key(&file.key())
                    && if self.has_log_selection() {
                        self.is_log_selected(file)
                    } else {
                        selected_port.as_deref().is_some_and(|port| {
                            Self::file_port(file) == Some(port)
                                && matches!(
                                    presentation::source(&file.name).0,
                                    Kind::Error | Kind::Access
                                )
                        })
                    }
            })
            .filter(|file| seen_sources.insert(self.snapshots[&file.key()].source_identity()))
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
            window,
            self.window.contains(None),
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
        self.activity_graph.cancel_selection();
        if window != self.window {
            self.undo_window = Some(self.window);
        }
        self.install_window(window, anchor);
    }

    fn install_window(&mut self, window: TimeWindow, anchor: Option<NaiveDateTime>) {
        self.window = window;
        self.default_truncation = false;
        self.automatic_window = false;
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
        self.truncation_dialog_opened_at = Some(Instant::now());
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
        self.truncation_dialog_opened_at = None;
        if let Some(dialog) = self.truncation_dialog.take() {
            self.truncation_draft = Some(dialog);
        }
    }

    pub(crate) fn handle_quick_clear_truncation(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> bool {
        use crossterm::event::{KeyCode, KeyModifiers};
        let opened = self.truncation_dialog_opened_at.take();
        if opened.is_some_and(|time| time.elapsed() < TRUNCATION_KEY_TIMEOUT)
            && key.code == KeyCode::Char('t')
            && (key.modifiers.is_empty() || key.modifiers == KeyModifiers::CONTROL)
        {
            let anchor = self.ensure_cursor_time();
            self.truncation_dialog = None;
            self.apply_window(TimeWindow::default(), anchor);
            true
        } else {
            false
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

    /// Export the displayed records, retaining calendar, truncation, and search
    /// limits, without reading the editor's result back into the cache.
    pub fn editor_export(&self) -> Result<PathBuf> {
        let path = crate::external_editor::temp_editor_path(Some(Path::new("logs.txt")));
        let mut output = std::io::BufWriter::new(fs::File::create(&path)?);
        for (position, index) in self.matches.iter().enumerate() {
            if position > 0 {
                output.write_all(b"\n")?;
            }
            output.write_all(self.display_row(*index).to_string().as_bytes())?;
        }
        output.flush()?;
        Ok(path)
    }
    #[cfg(test)]
    pub fn cached_total(&self) -> u64 {
        self.snapshots.values().map(|s| s.text.len() as u64).sum()
    }

    pub fn refresh(&mut self, rt: &Runtime) {
        self.refresh_inventory(rt, true);
    }

    fn refresh_automatic(&mut self, rt: &Runtime) {
        if !self.automatic_collection_allowed() {
            return;
        }
        self.refresh_inventory(rt, false);
    }

    fn refresh_inventory(&mut self, rt: &Runtime, explicit: bool) {
        if self.busy {
            self.refresh_queued = true;
            self.message = "Discovery queued until the current log sync finishes…".into();
            return;
        }
        let Some(client) = self.client.clone() else {
            return;
        };
        self.inventoried_hosts.clear();
        let progress = self.begin_activity("Discovering logs", 0);
        self.message = "Discovering hosts and log sizes…".into();
        let tx = self.tx.clone();
        let directory = self.directory.clone();
        let counter = self.counter.clone();
        let tracked_names = self.tracked_names();
        let receipts = self.history_state.inventories.clone();
        let fallback_day = self.now().date();
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
            let server_day = DateTime::parse_from_rfc3339(&discovery.now)
                .map(|time| time.date_naive())
                .unwrap_or(fallback_day);
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
                let days_ago = if !explicit
                    && receipts
                        .get(&endpoint.host)
                        .is_some_and(|receipt| receipt.covers(server_day, &tracked_names))
                {
                    0
                } else {
                    u32::MAX
                };
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
        if !self.has_downloadable_file(&file.name) {
            self.message = "No non-empty remote log is available to download.".into();
            return;
        }
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

    fn has_downloadable_file(&self, name: &str) -> bool {
        self.files
            .iter()
            .any(|file| file.name == name && file.present && file.size > 0)
    }

    fn start_full_download(&mut self, rt: &Runtime, name: &str) {
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for source in self
            .files
            .iter()
            .filter(|source| source.name == name && source.present && source.size > 0)
        {
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
        self.message = format!("Downloading {} on available hosts.", name);
        self.start_downloads(rt, plans, "Downloading logs");
    }

    /// Download the selected filename in full across available hosts without changing tracking.
    pub fn download(&mut self, rt: &Runtime) {
        let Some(file) = self.selected_file().cloned() else {
            self.message = "No log is selected.".into();
            return;
        };
        if !self.has_downloadable_file(&file.name) {
            self.message = "No non-empty remote log is available to download.".into();
        } else if self.storage_busy {
            self.message = "Log storage is busy; try the download again shortly.".into();
        } else if self.busy
            && self.activity.is_some()
            && !self.foreground_collection()
            && !self.endpoints.is_empty()
        {
            self.start_file_now(rt, &file.name);
        } else if self.busy {
            if self.activity.as_ref().is_some_and(|activity| {
                activity
                    .jobs
                    .iter()
                    .any(|((_, name), job)| name == &file.name && !job.done)
            }) {
                self.message = format!(
                    "Already downloading {}. Q: progress; Ctrl+D: restart now.",
                    file.name
                );
            } else {
                self.manual_downloads_queued.insert(file.name.clone());
                self.message = format!(
                    "Download queued: {}. Q: queue; Ctrl+D: start now.",
                    file.name
                );
            }
        } else {
            self.start_file_now(rt, &file.name);
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
            if !file.present
                || file.size == 0
                || Self::file_port(file) != Some(port)
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
        if self.storage_busy {
            return;
        }
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
        let cache_root = self.cache_root.clone();
        let handle = rt.spawn(async move {
            let mut tasks = JoinSet::new();
            let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_DOWNLOADS));
            for (_, endpoint, file, request, local, response_size) in jobs {
                let (tx, directory, counter, progress, permits, cache_root) = (
                    tx.clone(),
                    directory.clone(),
                    counter.clone(),
                    progress.clone(),
                    permits.clone(),
                    cache_root.clone(),
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
                    let result = match collect_since(&endpoint, &directory, &[request], "").await {
                        Ok(response) => {
                            if response.chunks.iter().any(|chunk| {
                                chunk.full
                                    && chunk.reset
                                    && chunk.text.len() as u64 > BACKGROUND_PARSE_BYTES
                            }) {
                                let _ = tx.send(CollectorEvent::DownloadStaging {
                                    host: endpoint.host.clone(),
                                    file: file.clone(),
                                });
                            }
                            let cancelled = Arc::new(AtomicBool::new(false));
                            let _guard = StageCancellation(cancelled.clone());
                            tokio::task::spawn_blocking(move || {
                                stage_download(response, &cache_root, &cancelled)
                            })
                            .await
                            .map_err(|error| format!("Cache staging worker failed: {error}"))
                            .and_then(|result| {
                                result.map_err(|error| format!("Cache staging: {error:#}"))
                            })
                        }
                        Err(error) => Err(format!("{error:#}")),
                    };
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
                || (only.is_none() && Self::file_days_ago(file) != Some(0))
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

    fn history_due(&self) -> bool {
        self.client.is_some()
            && self.automatic_collection_allowed()
            && self.now().hour() >= 1
            && self.history_checked != Some(self.now().date())
            && self
                .history_retry
                .is_none_or(|retry| Instant::now() >= retry)
    }

    fn historical_work(&self) -> (Vec<(LogFile, Snapshot)>, Vec<(Endpoint, Vec<Value>)>) {
        let mapped = self.grid_snapshots();
        let mut promotions = Vec::new();
        let mut requests: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        for file in &self.files {
            if (!file.present && !self.historical_path_known_present(file))
                || !Self::file_days_ago(file).is_some_and(|day| day > 0)
                || self.tracking_mode(file) == TrackingMode::Untracked
            {
                continue;
            }
            let source = mapped.get(&file.key()).copied();
            let complete = source.is_some_and(|snapshot| {
                snapshot.full_size == Some(file.size)
                    && snapshot.text_bytes == file.size
                    && history::same_day(&snapshot.file, file, self.server_offset)
                    && (snapshot.settled(self.server_offset)
                        || snapshot.open_content(&self.cache_root).is_ok())
            });
            if complete {
                let source = source.unwrap();
                if source.file.key() != file.key() {
                    promotions.push((file.clone(), source.meta()));
                }
                continue;
            }
            requests.entry(file.host.clone()).or_default().push(json!({
                "path": file.path, "offset": -1, "anchor_start": 0, "anchor": "", "full": true
            }));
        }
        // Preserve older rotations before replacing a filename that is still
        // the source for another copy (for example _1 -> _2 before current -> _1).
        promotions.sort_by_key(|(file, _)| std::cmp::Reverse(Self::file_days_ago(file)));
        let plans = self
            .endpoints
            .iter()
            .filter_map(|endpoint| {
                requests
                    .remove(&endpoint.host)
                    .map(|requests| (endpoint.clone(), requests))
            })
            .collect();
        (promotions, plans)
    }

    fn start_historical_catchup(&mut self, rt: &Runtime) {
        if !self.history_due() || self.busy {
            return;
        }
        if self.endpoints.is_empty()
            || self
                .endpoints
                .iter()
                .any(|endpoint| !self.inventoried_hosts.contains(&endpoint.host))
        {
            self.history_retry = Some(Instant::now() + Duration::from_secs(300));
            self.message = "Historical log check will retry after host discovery succeeds.".into();
            return;
        }
        let day = self.now().date();
        let (promotions, plans) = self.historical_work();
        if !promotions.is_empty() {
            self.history_running = Some(day);
            let progress =
                self.begin_activity("Updating local history references", promotions.len());
            self.activity.as_mut().unwrap().work_bytes = Some(
                promotions
                    .iter()
                    .map(|(_, snapshot)| snapshot.text_bytes)
                    .sum(),
            );
            let cancelled = Arc::new(AtomicBool::new(false));
            self.history_cancel = Some(cancelled.clone());
            let tx = self.tx.clone();
            let root = self.cache_root.clone();
            let handle = rt.spawn(async move {
                let tx_done = tx.clone();
                let result = tokio::task::spawn_blocking(move || {
                    for (file, snapshot) in promotions {
                        if cancelled.load(Ordering::Relaxed) {
                            break;
                        }
                        let _ = tx.send(CollectorEvent::DownloadStarted {
                            host: file.host.clone(),
                            file: file.name.clone(),
                        });
                        let result =
                            promote_rotated_cache(&root, &file, &snapshot, &cancelled, &progress)
                                .map(Box::new)
                                .map_err(|error| format!("{error:#}"));
                        let _ = tx.send(CollectorEvent::HistoryPromoted {
                            host: file.host.clone(),
                            file: file.name.clone(),
                            result,
                        });
                    }
                })
                .await;
                if let Err(error) = result {
                    let _ = tx_done.send(CollectorEvent::HistoryPromoted {
                        host: "local cache".into(),
                        file: "rotation".into(),
                        result: Err(format!("Cache worker failed: {error}")),
                    });
                }
                let _ = tx_done.send(CollectorEvent::Done { auto_tail: false });
            });
            self.abort = Some(handle.abort_handle());
        } else if !plans.is_empty() {
            self.history_running = Some(day);
            self.start_downloads(rt, plans, "Completing historical logs");
        } else {
            self.mark_history_checked(day);
            self.history_retry = None;
        }
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
                    None => {
                        Self::file_days_ago(file) == Some(0)
                            && self.tracking_mode(file) == TrackingMode::Live
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
        if self.storage_busy {
            return;
        }
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
        if self.storage_busy {
            return;
        }
        if self.cache_load.is_some() {
            self.poll_cache_load(rt);
            return;
        }
        if self
            .preview_changed
            .is_some_and(|changed| changed.elapsed() >= PREVIEW_DELAY)
            && visible
        {
            self.flush_file_preview();
        }
        if self
            .rendered_day
            .is_some_and(|day| day != self.discovery_day())
        {
            self.rebuild_lines();
        }
        if visible {
            if let Some((start, end)) = self.activity_graph.take_ready_window() {
                let anchor = self.ensure_cursor_time();
                self.apply_window(
                    TimeWindow {
                        start: Some(start),
                        end: Some(end),
                    },
                    anchor,
                );
            }
        } else {
            self.activity_graph.cancel_selection();
        }
        self.expire_truncation_sequence();
        self.tick_filter();
        if self
            .search_changed_at
            .is_some_and(|changed| changed.elapsed() >= PREVIEW_DELAY)
        {
            self.flush_search();
        }
        self.tick_index_filter();
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
                CollectorEvent::DownloadStaging { host, file } => {
                    if let Some(activity) = &mut self.activity {
                        activity.staging.insert((host, file));
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
                        Ok(response) => {
                            self.note_service_success();
                            match self.apply(response) {
                                Ok(()) => self.errors.remove(&host),
                                Err(error) => Some(format!("Cache error: {error:#}")),
                            }
                        }
                        Err(error) => Some(error),
                    };
                    if let Some(activity) = &mut self.activity {
                        activity.active.remove(&(host.clone(), file.clone()));
                        activity.staging.remove(&(host.clone(), file.clone()));
                        if let Some(job) = activity.jobs.get_mut(&(host.clone(), file.clone())) {
                            job.done = true;
                        }
                        activity.completed += 1;
                        activity.failed += usize::from(failure.is_some());
                        activity.last = Some((host.clone(), file.clone(), failure.is_some()));
                    }
                    let current = failure.map(|error| {
                        self.note_service_failure(&error);
                        format!("{file}: {error}")
                    });
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
                CollectorEvent::HistoryPromoted { host, file, result } => {
                    self.invalidate_sidebar_status();
                    let failed = match result {
                        Ok(snapshot) => {
                            self.snapshots.insert(snapshot.file.key(), *snapshot);
                            None
                        }
                        Err(error) => Some(error),
                    };
                    if let Some(activity) = &mut self.activity {
                        activity.active.remove(&(host.clone(), file.clone()));
                        activity.completed += 1;
                        activity.failed += usize::from(failed.is_some());
                        activity.last = Some((host.clone(), file.clone(), failed.is_some()));
                    }
                    if let Some(error) = failed {
                        self.errors.insert(host, format!("{file}: {error}"));
                    }
                }
                CollectorEvent::Endpoints(endpoints) => {
                    if let Some(activity) = &mut self.activity {
                        activity.total = endpoints.len();
                    }
                    self.endpoints = endpoints;
                }
                CollectorEvent::Clock(now) => {
                    self.note_service_success();
                    if let Ok(now) = DateTime::parse_from_rfc3339(&now) {
                        let offset_changed = self.server_offset != *now.offset();
                        if offset_changed {
                            self.invalidate_sidebar_status();
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
                    if let Some(error) = &chunk.error {
                        self.note_service_failure(error);
                    } else {
                        self.note_service_success();
                    }
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
                            self.note_service_success();
                            if let Err(e) = self.apply(response) {
                                self.errors.insert(host, format!("Cache error: {e:#}"));
                            }
                        }
                        Err(error) => {
                            self.note_service_failure(&error);
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
                            self.note_service_success();
                            if show_untracked {
                                let stale: BTreeSet<_> = self
                                    .files
                                    .iter()
                                    .filter(|file| {
                                        file.host == host
                                            && (days_ago == u32::MAX
                                                || self.history_inventory_known(&host)
                                                || self.tracking_mode(file)
                                                    == TrackingMode::Untracked)
                                            && (days_ago == u32::MAX
                                                || Self::file_days_ago(file) == Some(days_ago))
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
                                self.inventoried_hosts.remove(&host);
                                self.errors.insert(host, format!("Cache error: {error:#}"));
                            } else if show_untracked {
                                if days_ago == u32::MAX {
                                    self.record_history_inventory(&host);
                                }
                                if days_ago == u32::MAX || self.history_inventory_known(&host) {
                                    self.inventoried_hosts.insert(host);
                                }
                            }
                        }
                        Err(error) => {
                            self.note_service_failure(&error);
                            self.inventoried_hosts.remove(&host);
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
                    let activity_label = finished.as_ref().map(|activity| activity.label);
                    let mut continue_history = false;
                    if matches!(
                        activity_label,
                        Some("Updating local history references" | "Completing historical logs")
                    ) {
                        self.history_cancel = None;
                        if finished
                            .as_ref()
                            .is_some_and(|activity| activity.failed > 0)
                        {
                            self.history_running = None;
                            self.history_retry = Some(Instant::now() + Duration::from_secs(300));
                        } else if activity_label == Some("Updating local history references") {
                            self.history_running = None;
                            continue_history = true;
                        } else {
                            if let Some(day) = self.history_running.take() {
                                self.mark_history_checked(day);
                            }
                            self.history_retry = None;
                        }
                    } else if activity_label == Some("Discovering logs") && !refresh_queued {
                        continue_history = true;
                    }
                    if !refresh_queued && activity_label == Some("Discovering logs") {
                        self.day_selection = None;
                    }
                    self.start_queued_downloads(rt);
                    if refresh_queued {
                        if self.busy {
                            self.refresh_queued = true;
                        } else {
                            self.refresh(rt);
                        }
                    }
                    if !self.busy && continue_history {
                        self.start_historical_catchup(rt);
                    }
                    if !self.busy
                        && auto_tail
                        && self.automatic_collection_allowed()
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
        let mut page_edge = None;
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
                    let recovered = self.read_failures.remove(&result.key).is_some();
                    if recovered
                        && self.read_failures.is_empty()
                        && self.message.starts_with("Local log read failed:")
                    {
                        self.message = "Local log read completed.".into();
                    }
                    if self.snapshots.get(&result.key).is_some_and(|snapshot| {
                        snapshot.archive.is_none() && snapshot.settled(self.server_offset)
                    }) {
                        self.archive_dirty = true;
                    }
                    if let Some(page) = self.page_navigation.as_mut().filter(|page| {
                        page.key == result.key
                            && page.generation == result.spec.generation
                            && page.window == result.spec.window
                    }) && let Some((cursors, older)) = page.pending.take()
                    {
                        if cursors.last().copied().flatten() == result.spec.before {
                            page.cursors = cursors;
                            page_edge = Some(older);
                        }
                    }
                    self.parsed_streams.insert(result.key.clone(), stream);
                    self.parsed_large
                        .insert(result.key, (result.spec, truncated));
                    if let Some(reason) = result.fallback_reason {
                        self.message = reason;
                    } else if truncated {
                        self.message = if result.spec.paged { "More indexed records are available." } else { "Large log view limited to recent records; narrow the time window to see earlier entries." }.into();
                    }
                    parsed = true;
                }
                Err(error) if error.kind() != std::io::ErrorKind::Interrupted => {
                    self.read_failures.insert(
                        result.key.clone(),
                        ReadFailure {
                            spec: result.spec,
                            at: Instant::now(),
                        },
                    );
                    if let Some(page) = self
                        .page_navigation
                        .as_mut()
                        .filter(|page| page.key == result.key)
                    {
                        page.pending = None;
                    }
                    self.message =
                        format!("Local log read failed: {error} · retries paused for 60s");
                }
                Err(_) => {}
            }
        }
        if parsed {
            self.sidebar_rows.borrow_mut().clear();
            if page_edge.is_some() {
                self.cursor_time = None;
                self.host_anchor_position = None;
            }
            self.rebuild_lines();
            if let Some(older) = page_edge {
                self.cursor_time = None;
                self.move_host_anchor_to_edge(older);
            } else if let Some(time) = self.cursor_time {
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
        self.start_queued_downloads(rt);
        if !self.identity.is_empty() {
            self.tick_archive(rt);
        }
        if !self.busy && self.history_due() {
            self.refresh_automatic(rt);
        }
        if !self.busy && self.automatic_collection_allowed() && self.last_poll.elapsed() >= INTERVAL
        {
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
        self.commit_with_staged(file, fresh, text, None, update)
    }

    fn commit_with_staged(
        &mut self,
        file: LogFile,
        fresh: bool,
        text: &str,
        staged: Option<&StagedChunk>,
        update: impl FnOnce(&mut Snapshot),
    ) -> Result<usize> {
        self.archive_dirty = true;
        self.invalidate_sidebar_status();
        if self.storage_busy {
            bail!("Storage operation in progress; retry after it finishes");
        }
        if staged.is_some() && !fresh {
            bail!("Only replacement generations can be staged");
        }
        fs::create_dir_all(&self.cache_root)?;
        let _publication = ArchivePublishLock::acquire(&self.cache_root)?;
        let key = file.key();
        let stem = hash_key(&key);
        let prior = self.snapshots.get(&key);
        if !fresh
            && let Some(previous) = prior.filter(|snapshot| snapshot.archive.is_some())
            && !fs::metadata(previous.content_path(&self.cache_root))
                .is_ok_and(|metadata| metadata.len() >= previous.text_bytes)
        {
            bail!("Cannot append to an archived log without its raw generation; download it again");
        }
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
                    archive: None,
                    storage: None,
                    time: None,
                    full_size: None,
                    updated_at: None,
                },
                0,
            ),
        };
        if !fresh {
            meta.archive = None;
            if !text.is_empty() {
                meta.full_size = None;
            }
        }
        meta.file = file;
        update(&mut meta);
        let content_bytes = staged.map_or(text.len() as u64, |stage| stage.bytes);
        meta.text_bytes = (base as u64)
            .checked_add(content_bytes)
            .context("Cached log size overflow")?;
        fs::create_dir_all(&self.cache_root)?;
        let log = meta.content_path(&self.cache_root);
        let metadata_path = self.cache_root.join(format!("{stem}.json"));
        if !fresh && prior.is_some_and(|snapshot| snapshot.storage.is_some()) {
            let previous = prior.unwrap();
            let appended = storage::append(&self.cache_root, previous, text)?;
            meta.storage = Some(appended.name.clone());
            if !text.is_empty() {
                meta.updated_at = Some(Utc::now().to_rfc3339());
            }
            let durable = storage::save(&metadata_path, &serde_json::to_vec(&meta)?)?;
            appended.published(durable);
        } else if fresh {
            // Write a replacement beside the active generation. The manifest
            // switches only after all new bytes are present, so old readers and
            // a restarted process can continue using the prior generation.
            let stage = self
                .cache_root
                .join(format!("{stem}.{}.staging", meta.generation));
            let mut managed_durable = false;
            let result = (|| -> Result<()> {
                if let Some(prepared) = staged {
                    fs::rename(&prepared.path, &log)?;
                } else {
                    let mut out = fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .truncate(true)
                        .open(&stage)?;
                    out.write_all(text.as_bytes())?;
                    out.sync_all()?;
                    drop(out);
                    fs::rename(&stage, &log)?;
                }
                if fresh || !text.is_empty() {
                    meta.updated_at = Some(Utc::now().to_rfc3339());
                }
                if prior.is_some_and(|snapshot| snapshot.storage.is_some()) {
                    managed_durable = storage::save(&metadata_path, &serde_json::to_vec(&meta)?)?;
                } else {
                    atomic_write(&metadata_path, &serde_json::to_vec(&meta)?)?;
                }
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
                if previous.storage.is_some() && managed_durable {
                    let warnings = storage::retire_managed(&self.cache_root, previous);
                    if !warnings.is_empty() {
                        self.message =
                            format!("Old daily storage retained: {}", warnings.join(" | "));
                    }
                }
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
        let selected = self.selected_file();
        let changed = self.files.iter().find(|file| file.key() == key);
        if self.all_cached
            || selected.zip(changed).is_some_and(|(selected, changed)| {
                Self::file_port(selected) == Self::file_port(changed)
                    && Self::log_kind_sort_key(selected) == Self::log_kind_sort_key(changed)
            })
        {
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
        self.invalidate_sidebar_status();
        if size.is_none() && modified.is_none() {
            return Ok(());
        }
        if self.storage_busy {
            bail!("Storage operation in progress; retry after it finishes");
        }
        fs::create_dir_all(&self.cache_root)?;
        let _storage_lock = ArchivePublishLock::acquire(&self.cache_root)?;
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
            let content_bytes = chunk
                .staged
                .as_ref()
                .map_or(chunk.text.len() as u64, |stage| stage.bytes);
            let old_len =
                self.commit_with_staged(file, fresh, &chunk.text, chunk.staged.as_ref(), |s| {
                    if fresh {
                        s.start = chunk.start;
                    }
                    s.end = chunk.end;
                    s.length = chunk.length;
                    s.anchor_start = chunk.anchor_start;
                    s.anchor = chunk.anchor.clone();
                    if chunk.full && content_bytes == s.file.size {
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
                &serde_json::to_vec(
                    &self
                        .files
                        .iter()
                        .filter(|file| !self.retained_archive_days.contains_key(&file.key()))
                        .collect::<Vec<_>>(),
                )?,
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

/// Preserve an already downloaded rotation under its new name without fetching it again.
fn promote_rotated_cache(
    root: &Path,
    file: &LogFile,
    source: &Snapshot,
    cancelled: &AtomicBool,
    progress: &AtomicU64,
) -> Result<Snapshot> {
    fs::create_dir_all(root)?;
    let _lock = ArchivePublishLock::acquire(root)?;
    let source_path = root.join(format!("{}.json", hash_key(&source.file.key())));
    let source_metadata = fs::read(&source_path)?;
    let disk: Snapshot = serde_json::from_slice(&source_metadata)?;
    if disk.generation != source.generation
        || disk.text_bytes != source.text_bytes
        || disk.full_size != source.full_size
        || disk.updated_at != source.updated_at
        || disk.storage != source.storage
        || disk.archive != source.archive
    {
        bail!("Cached source changed; retry the historical check");
    }
    let target_path = root.join(format!("{}.json", hash_key(&file.key())));
    let target_metadata = fs::read(&target_path).ok();
    let previous: Option<Snapshot> = target_metadata
        .as_deref()
        .and_then(|bytes| serde_json::from_slice(bytes).ok());
    if let Some(previous) = &previous {
        if previous.full_size == Some(file.size)
            && previous.text_bytes == file.size
            && LogViewer::same_modified(&previous.file.modified, &file.modified)
            && previous.open_content(root).is_ok()
        {
            return Ok(previous.meta());
        }
    }
    let mut target = source.meta();
    target.file = file.clone();
    target.generation = previous.map_or(0, |prior| prior.generation.saturating_add(1));
    target.generation_path = true;
    if source.archive.is_some()
        && source.full_size == Some(source.text_bytes)
        && source.text_bytes == file.size
    {
        if cancelled.load(Ordering::Relaxed) {
            bail!("Historical reference update cancelled");
        }
        // Separate names share immutable bytes, so deleting one source cannot
        // remove another rotation's archive. No decompression or byte copy.
        let source_path = source.archive_path(root).context("Invalid archive path")?;
        let name = format!(
            "{}-{}-ref-{}-{}.gz",
            hash_key(&file.key()),
            target.generation,
            history::archive_identity(source.archive.as_deref().unwrap()),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let destination = root.join("archives").join(&name);
        fs::hard_link(&source_path, &destination)?;
        let mut artifact = ArchiveArtifact {
            path: destination.clone(),
            keep: false,
        };
        let sidecar = |path: &Path| {
            let mut path = path.as_os_str().to_os_string();
            path.push(".index.json");
            PathBuf::from(path)
        };
        fs::hard_link(sidecar(&source_path), sidecar(&destination))?;
        target.archive = Some(name);
        let _ = fs::hard_link(source.index_path(root), target.index_path(root));
        fs::File::open(destination.parent().context("No archive directory")?)?.sync_all()?;
        atomic_write(&target_path, &serde_json::to_vec(&target)?)?;
        artifact.keep = true;
        progress.fetch_add(source.text_bytes, Ordering::Relaxed);
        return Ok(target);
    }
    target.archive = None;
    target.storage = None;
    target.text.clear();
    let destination = target.content_path(root);
    let stage = root.join(format!(
        "{}.{}.history-{}.staging",
        hash_key(&file.key()),
        target.generation,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    // Copy the pinned source length without excluding downloads and archiving.
    // Revalidate both metadata files under the lock before publishing.
    drop(_lock);
    let result = (|| -> Result<()> {
        let mut input = source.open_content(root)?.take(source.text_bytes);
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        let mut buffer = vec![0u8; 128 * 1024];
        let mut copied = 0u64;
        loop {
            if cancelled.load(Ordering::Relaxed) {
                bail!("Historical cache copy cancelled");
            }
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            output.write_all(&buffer[..count])?;
            copied += count as u64;
            progress.fetch_add(count as u64, Ordering::Relaxed);
        }
        if copied != source.text_bytes {
            bail!("Cached source is incomplete");
        }
        output.sync_all()?;
        drop(output);
        if cancelled.load(Ordering::Relaxed) {
            bail!("Historical cache copy cancelled");
        }
        let _publication = ArchivePublishLock::acquire(root)?;
        if fs::read(&source_path)? != source_metadata
            || fs::read(&target_path).ok() != target_metadata
        {
            bail!("Cached source or target changed; retry the historical check");
        }
        fs::rename(&stage, &destination)?;
        if let Err(error) = atomic_write(&target_path, &serde_json::to_vec(&target)?) {
            let _ = fs::remove_file(&destination);
            return Err(error.into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result?;
    Ok(target)
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
        "days-ago": if days_ago == u32::MAX { "-1".to_owned() } else { days_ago.to_string() },
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

async fn archive_remote_unchanged(
    endpoint: &Endpoint,
    directory: &str,
    file: &LogFile,
) -> Result<()> {
    let day = LogViewer::file_days_ago(file).context("Unknown log rotation")?;
    let inventory = inventory(endpoint, directory, day, true, &BTreeSet::new()).await?;
    let current = inventory
        .files
        .iter()
        .find(|current| current.path == file.path)
        .context("Remote log is no longer available")?;
    if current.size != file.size || current.modified != file.modified {
        bail!("Remote log changed since the complete download");
    }
    Ok(())
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

/// Stage large replacement bodies away from the terminal thread. The old
/// generation remains authoritative until the event loop publishes metadata.
fn stage_download(
    mut response: HostResponse,
    root: &Path,
    cancelled: &AtomicBool,
) -> Result<HostResponse> {
    static NEXT_STAGE: AtomicU64 = AtomicU64::new(0);
    for chunk in &mut response.chunks {
        if !chunk.full || !chunk.reset || chunk.text.len() as u64 <= BACKGROUND_PARSE_BYTES {
            continue;
        }
        let file = response
            .files
            .iter()
            .find(|file| file.host == chunk.host && file.path == chunk.path)
            .context("Chunk has no matching file metadata")?;
        let mut chars = 0u64;
        for _ in chunk.text.chars() {
            chars += 1;
            if chars % 65_536 == 0 && cancelled.load(Ordering::Relaxed) {
                bail!("Cache staging cancelled");
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            bail!("Cache staging cancelled");
        }
        if chunk.end < chunk.start || chunk.end > chunk.length || chars != chunk.end - chunk.start {
            bail!("Invalid log chunk range for {}", file.path);
        }
        let dir = root.join(".staging");
        fs::create_dir_all(&dir)?;
        let (path, mut output) = loop {
            let id = NEXT_STAGE.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{id}.log", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(output) => break (path, output),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        };
        let staged = StagedChunk {
            path,
            bytes: chunk.text.len() as u64,
            chars,
        };
        for block in chunk.text.as_bytes().chunks(1024 * 1024) {
            if cancelled.load(Ordering::Relaxed) {
                bail!("Cache staging cancelled");
            }
            output.write_all(block)?;
        }
        if cancelled.load(Ordering::Relaxed) {
            bail!("Cache staging cancelled");
        }
        output.sync_all()?;
        drop(output);
        chunk.staged = Some(staged);
        chunk.text = String::new();
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
    let chars = chunk
        .staged
        .as_ref()
        .map_or_else(|| chunk.text.chars().count() as u64, |stage| stage.chars);
    if chunk.end < chunk.start || chunk.end > chunk.length || chars != chunk.end - chunk.start {
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
            archive: None,
            storage: None,
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

    #[test]
    fn timeline_clicks_preview_then_apply_undoable_window_and_quick_clear() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let mut logs = viewer();
        let source = named_file("one", "8000_ErrorLog.txt", "2026-10-14T09:00:00Z");
        logs.files.push(source.clone());
        logs.commit(
            source,
            true,
            concat!(
                "2026-10-14 08:00:00.000 Info: before\n",
                "2026-10-14 08:02:00.000 Info: inside-one\n",
                "2026-10-14 08:05:00.000 Info: inside-two\n",
                "2026-10-14 08:10:00.000 Info: after\n",
            ),
            |_| {},
        )
        .unwrap();
        logs.sidebar_visible = false;
        logs.files_focus = false;
        logs.select(0);
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let mut terminal = Terminal::new(TestBackend::new(80, 30)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            terminal
                .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
                .unwrap();
            if app.server_logs.activity_graph.chart.is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "activity chart did not load");
            std::thread::sleep(Duration::from_millis(1));
        }
        let area = app
            .server_logs
            .mouse_regions
            .iter()
            .find_map(|(area, target)| matches!(target, LogMouseTarget::Activity).then_some(*area))
            .unwrap();
        let left = area.width / 6;
        let right = area.width * 2 / 3;
        let click = |app: &mut crate::App, column| {
            crate::events::handle_mouse_event(
                app,
                MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: area.x + column,
                    row: area.y + 1,
                    modifiers: KeyModifiers::NONE,
                },
            )
            .unwrap();
        };
        click(&mut app, right);
        click(&mut app, left);
        crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(app.server_logs.activity_graph.selection_columns().is_none());
        assert_eq!(app.server_logs.window, TimeWindow::default());
        click(&mut app, right);
        click(&mut app, left);
        assert_eq!(app.server_logs.window, TimeWindow::default());
        app.server_logs.tick(&app.rt, true);
        assert_eq!(app.server_logs.window, TimeWindow::default());
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(area.x + left, area.y + 1)].bg,
            Color::Rgb(75, 110, 165)
        );
        assert_eq!(
            buffer[(area.x + right, area.y + 2)].bg,
            Color::Rgb(75, 110, 165)
        );
        assert_ne!(
            buffer[(area.x + left - 1, area.y + 1)].bg,
            Color::Rgb(75, 110, 165)
        );

        app.server_logs.activity_graph.finish_preview_for_test();
        app.server_logs.tick(&app.rt, true);
        let zoom = app.server_logs.window;
        let parse = |time| presentation::parse_time(time, app.server_logs.server_offset).unwrap();
        assert!(zoom.start.unwrap() > parse("2026-10-14 08:00:00"));
        assert!(zoom.start.unwrap() < parse("2026-10-14 08:02:00"));
        assert!(zoom.end.unwrap() > parse("2026-10-14 08:05:00"));
        assert!(zoom.end.unwrap() < parse("2026-10-14 08:10:00"));
        let text = app.server_logs.visible_text();
        assert!(text.contains("inside-one") && text.contains("inside-two"));
        assert!(!text.contains("Info: before") && !text.contains("Info: after"));

        app.server_logs.undo_truncation();
        assert_eq!(app.server_logs.window, TimeWindow::default());
        app.server_logs.apply_window(zoom, None);

        let send = |app: &mut crate::App, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(KeyCode::Char('t'), modifiers))
                .unwrap();
        };
        send(&mut app, KeyModifiers::CONTROL);
        assert!(app.server_logs.truncation_dialog.is_some());
        send(&mut app, KeyModifiers::NONE);
        assert_eq!(app.server_logs.window, TimeWindow::default());
        assert!(app.server_logs.truncation_dialog.is_none());
        app.server_logs.undo_truncation();
        assert_eq!(app.server_logs.window, zoom);
        send(&mut app, KeyModifiers::CONTROL);
        send(&mut app, KeyModifiers::CONTROL);
        assert_eq!(app.server_logs.window, TimeWindow::default());
        app.server_logs.undo_truncation();
        assert_eq!(app.server_logs.window, zoom);
        app.server_logs.undo_truncation();
        assert_eq!(app.server_logs.window, zoom); // one undo level
        send(&mut app, KeyModifiers::CONTROL);
        app.server_logs.truncation_dialog_opened_at = Some(Instant::now() - TRUNCATION_KEY_TIMEOUT);
        send(&mut app, KeyModifiers::NONE);
        assert!(app.server_logs.truncation_dialog.is_some());
        assert_eq!(app.server_logs.window, zoom);
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

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
            staged: None,
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

    fn viewer_on(date: &str) -> LogViewer {
        let mut logs = viewer();
        logs.clock = Some((
            NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        logs
    }

    #[test]
    fn browsing_coalesces_previews_and_reads_only_the_final_selection() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        for (port, message) in [(8000, "first"), (8001, "middle"), (8002, "final")] {
            let source = named_file(
                "one",
                &format!("{port}_ErrorLog.txt"),
                "2026-10-14T09:00:00Z",
            );
            logs.modes
                .insert(source.name.clone(), TrackingMode::OnDemand);
            logs.files.push(source.clone());
            logs.commit(
                source,
                true,
                &format!("2026-10-14 08:00:00.000 Info: {message}\n"),
                |_| {},
            )
            .unwrap();
        }
        logs.select(0);
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let down = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
        crate::events::handle_server_logs_key(&mut app, down).unwrap();
        crate::events::handle_server_logs_key(&mut app, down).unwrap();
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8002_ErrorLog.txt"
        );
        assert!(app.server_logs.preview_pending());
        assert_eq!(
            app.server_logs.parsed_streams.len(),
            1,
            "intermediate selections must not be parsed"
        );
        app.server_logs.tick(&app.rt, true);
        assert!(
            app.server_logs.preview_pending(),
            "the delay restarts on each navigation event"
        );

        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Preview updates when browsing pauses"));
        assert!(
            !screen.contains("Info: first"),
            "don't label old content as the new selection"
        );

        app.server_logs.preview_changed = Some(Instant::now() - PREVIEW_DELAY);
        app.server_logs.tick(&app.rt, true);
        assert!(!app.server_logs.preview_pending());
        assert!(app.server_logs.visible_text().contains("final"));
        let middle = &app.server_logs.files[1];
        assert!(!app.server_logs.parsed_streams.contains_key(&middle.key()));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn enter_opens_a_pending_preview_without_waiting_or_downloading() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        app.server_logs
            .apply(response(
                "one",
                vec![chunk(
                    "one",
                    0,
                    "2026-10-14 08:00:00.000 Info: cached\n",
                    true,
                )],
            ))
            .unwrap();
        app.server_logs.defer_file_preview();
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert!(!app.server_logs.preview_pending());
        assert!(!app.server_logs.files_focus);
        assert!(!app.server_logs.busy);
        assert!(app.server_logs.visible_text().contains("cached"));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn small_cached_search_debounces_typing_and_enter_applies_immediately() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        app.server_logs
            .apply(response(
                "one",
                vec![chunk(
                    "one",
                    0,
                    "2026-10-14 08:00:00.000 Info: alpha\n2026-10-14 08:01:00.000 Info: beta\n",
                    true,
                )],
            ))
            .unwrap();
        app.server_logs.files_focus = false;
        app.server_logs.search_editing = true;
        let previous = app.server_logs.matches.clone();
        for character in "alpha".chars() {
            crate::events::handle_server_logs_key(
                &mut app,
                KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE),
            )
            .unwrap();
            assert_eq!(app.server_logs.matches, previous);
        }
        app.server_logs.search_changed_at = Some(Instant::now() - PREVIEW_DELAY);
        app.server_logs.tick(&app.rt, true);
        assert!(app.server_logs.visible_text().contains("alpha"));
        assert!(!app.server_logs.visible_text().contains("beta"));
        app.server_logs.search = "beta".into();
        app.server_logs.search_changed();
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.server_logs.visible_text().contains("beta"));
        assert!(!app.server_logs.visible_text().contains("alpha"));
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn staged_download_publishes_unicode_bytes_as_a_new_generation() {
        let mut logs = viewer();
        let source = file("one");
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old content\n", |_| {})
            .unwrap();
        let old_path = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let text = format!("{}\n", "é".repeat(530_000));
        let mut chunk = chunk("one", 0, &text, true);
        chunk.full = true;
        chunk.anchor.clear();
        let mut response = response("one", vec![chunk]);
        response.files[0].size = text.len() as u64;
        let response = stage_download(response, &logs.cache_root, &AtomicBool::new(false)).unwrap();
        assert!(response.chunks[0].text.is_empty());
        let stage = response.chunks[0].staged.as_ref().unwrap().path.clone();
        assert!(stage.exists());
        assert_eq!(fs::read_to_string(&old_path).unwrap(), "old content\n");

        logs.apply(response).unwrap();
        let snapshot = &logs.snapshots[&source.key()];
        assert_eq!(snapshot.full_size, Some(text.len() as u64));
        assert_eq!(snapshot.text_bytes, text.len() as u64);
        assert_eq!(
            fs::read(snapshot.content_path(&logs.cache_root)).unwrap(),
            text.as_bytes()
        );
        assert_eq!(fs::read_to_string(&old_path).unwrap(), "old content\n");
        assert!(!stage.exists());
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn discarded_staged_download_removes_temporary_file() {
        let logs = viewer();
        let text = "a".repeat(BACKGROUND_PARSE_BYTES as usize + 1);
        let mut first_chunk = chunk("one", 0, &text, true);
        first_chunk.full = true;
        first_chunk.anchor.clear();
        let prepared = stage_download(
            response("one", vec![first_chunk]),
            &logs.cache_root,
            &AtomicBool::new(false),
        )
        .unwrap();
        let stage = prepared.chunks[0].staged.as_ref().unwrap().path.clone();
        assert!(stage.exists());
        drop(prepared);
        assert!(!stage.exists());
        let mut cancelled_chunk = chunk("one", 0, &text, true);
        cancelled_chunk.full = true;
        cancelled_chunk.anchor.clear();
        assert!(
            stage_download(
                response("one", vec![cancelled_chunk]),
                &logs.cache_root,
                &AtomicBool::new(true),
            )
            .is_err()
        );
        assert_eq!(
            fs::read_dir(logs.cache_root.join(".staging"))
                .unwrap()
                .count(),
            0
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn download_status_shows_when_cache_is_being_saved() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        logs.begin_activity("Downloading logs", 1);
        logs.activity.as_mut().unwrap().download = true;
        logs.tx
            .send(CollectorEvent::DownloadStarted {
                host: "one".into(),
                file: "ErrorLog.txt".into(),
            })
            .unwrap();
        logs.tx
            .send(CollectorEvent::DownloadStaging {
                host: "one".into(),
                file: "ErrorLog.txt".into(),
            })
            .unwrap();
        logs.tick(&runtime, true);
        assert!(
            logs.activity_status()
                .unwrap()
                .contains("ErrorLog.txt (saving)")
        );
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn rotated_server_error_log_appears_in_the_unported_row() {
        let mut logs = viewer();
        logs.discovery_days_ago = 1;
        logs.files
            .push(named_file("one", "ErrorLog_1.txt", "2026-10-13T23:59:00Z"));
        let rows = logs.visible_port_rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "----");
        assert_eq!(rows[0].1[0], Some(0));
        fs::remove_dir_all(logs.cache_root).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn logs_command_paints_and_closes_while_cache_io_is_blocked() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::Normal;
        let client = test_client();
        let mut logs = viewer();
        logs.identity = format!(
            "{}\0{}\0{}\0",
            client.server.name,
            client.server.base_url(),
            client.server.username
        );
        logs.cache_load_needed = true;
        let catalog = logs.cache_root.join(CATALOG_FILE);
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&catalog)
                .status()
                .unwrap()
                .success()
        );
        let (painted, wait_for_paint) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            // Release even a broken synchronous opener, so this test fails
            // instead of hanging if the UI ever starts doing cache I/O again.
            let _ = wait_for_paint.recv_timeout(Duration::from_secs(2));
            fs::write(catalog, b"[]").unwrap();
        });
        app.server_logs = logs;
        app.client = Some(client);
        app.command_input = ":logs".into();
        let started = Instant::now();
        app.execute_command();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "opening must return before the blocked cache read"
        );
        assert_eq!(app.mode, crate::AppMode::MarkLogicLogs);
        assert!(app.server_logs.cache_loading());
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("loading local history"));
        assert!(screen.contains("Reading local log catalog and cache metadata"));
        assert!(screen.contains("x: cancel loading"));
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        )
        .unwrap();
        assert_eq!(
            app.mode,
            crate::AppMode::Normal,
            "closing must stay responsive during disk I/O"
        );
        painted.send(()).unwrap();
        app.server_logs.client = None; // Keep this disk/UI test independent of a server.
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.server_logs.cache_loading() {
            assert!(Instant::now() < deadline);
            app.server_logs.tick(&app.rt, false);
            std::thread::sleep(Duration::from_millis(1));
        }
        writer.join().unwrap();
        assert!(!app.server_logs.cache_load_needed);
        assert!(!app.server_logs.busy);
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn background_cache_load_failure_can_be_retried() {
        let mut logs = viewer();
        let runtime = Runtime::new().unwrap();
        fs::remove_dir_all(&logs.cache_root).unwrap();
        fs::write(&logs.cache_root, b"not a directory").unwrap();
        logs.cache_load_needed = true;
        logs.start_cache_load();
        let deadline = Instant::now() + Duration::from_secs(5);
        while logs.cache_loading() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!logs.busy);
        assert!(logs.cache_load_needed);
        assert!(logs.message.contains("Loading local history:"));
        fs::remove_file(&logs.cache_root).unwrap();
        logs.start_cache_load();
        let deadline = Instant::now() + Duration::from_secs(5);
        while logs.cache_loading() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!logs.cache_load_needed);
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
                    assert_eq!(vars["show-untracked"], "true");
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
    fn horizontal_navigation_prefers_the_same_stream_on_an_older_day() {
        let mut logs = viewer();
        for name in [
            "TaskServer_ErrorLog.txt",
            "TaskServer_ErrorLog_1.txt",
            "8000_RequestLog.txt",
        ] {
            logs.files
                .push(named_file("one", name, "2026-10-14T09:00:00Z"));
        }
        logs.modes
            .insert("8000_RequestLog.txt".into(), TrackingMode::OnDemand);
        logs.select(0);
        logs.select_grid_horizontal(true, false);
        assert_eq!(
            logs.selected_file().unwrap().name,
            "TaskServer_ErrorLog_1.txt"
        );
        assert_eq!(logs.discovery_days_ago, 1);
        logs.select_grid_horizontal(false, false);
        assert_eq!(
            logs.selected_file().unwrap().name,
            "TaskServer_ErrorLog.txt"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn historical_catchup_downloads_only_missing_files_and_finishes() {
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
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
            let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
            let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["path"], "/Logs/ErrorLog_2.txt");
            let reply = json!({"host":"one", "directory":"/Logs", "inventory":false, "read_bytes":4, "cluster_bytes":0,
                "files":[{"host":"one","path":"/Logs/ErrorLog_2.txt","name":"ErrorLog_2.txt","size":4,"modified":"2026-10-12T08:00:00Z"}],
                "chunks":[{"host":"one","path":"/Logs/ErrorLog_2.txt","start":0,"end":4,"length":4,"reset":true,"text":"old\n","full":true}]}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        logs.client = Some(client.clone());
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.inventoried_hosts.insert("one".into());
        logs.tracking_periods.insert(
            "ErrorLog.txt".into(),
            vec![TrackingPeriod {
                start: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
                end: None,
            }],
        );
        for (name, modified) in [
            ("ErrorLog_1.txt", "2026-10-13T08:00:00Z"),
            ("ErrorLog_2.txt", "2026-10-12T08:00:00Z"),
        ] {
            let mut file = named_file("one", name, modified);
            file.size = 4;
            logs.files.push(file);
        }
        let cached = logs.files[0].clone();
        logs.commit(cached, true, "old\n", |snapshot| {
            snapshot.full_size = Some(4)
        })
        .unwrap();
        logs.start_historical_catchup(&runtime);
        assert!(logs.busy);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while logs.busy {
                    logs.tick(&runtime, false);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        runtime.block_on(server).unwrap();
        assert_eq!(logs.history_checked, Some(logs.now().date()));
        assert!(logs.historical_work().1.is_empty());
        assert_eq!(
            logs.snapshots[&logs
                .files
                .iter()
                .find(|f| f.name == "ErrorLog_2.txt")
                .unwrap()
                .key()]
                .full_size,
            Some(4)
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn failed_historical_download_is_retried_instead_of_marked_complete() {
        let mut logs = viewer();
        let today = logs.now().date();
        logs.history_running = Some(today);
        logs.begin_activity("Completing historical logs", 1);
        logs.tx
            .send(CollectorEvent::DownloadReport {
                host: "one".into(),
                file: "ErrorLog_1.txt".into(),
                result: Err("host unavailable".into()),
                bytes: 0,
            })
            .unwrap();
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        let runtime = Runtime::new().unwrap();
        logs.tick(&runtime, false);
        assert_eq!(logs.history_checked, None);
        assert_eq!(logs.history_running, None);
        assert!(logs.history_retry.is_some());
        assert!(logs.errors["one"].contains("host unavailable"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn settled_history_requires_a_full_closed_download_after_its_day() {
        let mut logs = viewer();
        let source = named_file("one", "ErrorLog_1.txt", "2026-10-13T12:00:00Z");
        let mut source = source;
        source.size = 4;
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        let mut snapshot = logs.snapshots[&source.key()].meta();
        let offset = FixedOffset::east_opt(0).unwrap();
        assert!(snapshot.settled(offset));
        snapshot.updated_at = Some(source.modified.clone());
        assert!(
            !snapshot.settled(offset),
            "a download on the source day may still grow"
        );
        snapshot.updated_at = Some("2026-10-14T00:00:00Z".into());
        snapshot.full_size = None;
        assert!(
            !snapshot.settled(offset),
            "a later partial tail is still partial"
        );
        snapshot.full_size = Some(4);
        snapshot.start = 1;
        assert!(!snapshot.settled(offset));
        snapshot.start = 0;
        snapshot.file = file("one");
        snapshot.file.size = 4;
        assert!(
            !snapshot.settled(offset),
            "current filenames remain writable even when quiet"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn historical_checkpoint_and_confirmed_absence_survive_restart() {
        let mut logs = viewer();
        let mut missing = named_file("one", "ErrorLog_2.txt", "2026-10-12T12:00:00Z");
        missing.present = false;
        logs.files = vec![file("one"), missing.clone()];
        logs.record_history_inventory("one");
        logs.mark_history_checked(logs.now().date());
        let mut restored = viewer();
        fs::remove_dir_all(&restored.cache_root).unwrap();
        restored.cache_root = logs.cache_root.clone();
        restored.load_history_state();
        restored.client = Some(test_client());
        assert_eq!(restored.history_checked, Some(logs.now().date()));
        assert!(!restored.history_due());
        assert!(restored.history_inventory_known("one"));
        assert!(restored.history_missing(&missing));
        assert!(!restored.historical_path_known_present(&missing));
        restored.files = vec![missing.clone()];
        restored.files[0].present = true;
        restored.record_history_inventory("one");
        assert!(
            !restored.history_missing(&missing),
            "explicit discovery can correct an absence receipt"
        );
        restored.clock = Some((
            (logs.now().date() + Days::days(1))
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        assert!(
            restored.history_due(),
            "a new server day still needs discovery"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn saved_inventory_limits_automatic_discovery_to_today_but_explicit_refresh_checks_history() {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = runtime.spawn(async move {
            for step in 0..4 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") { header.push(stream.read_u8().await.unwrap()); }
                let length: usize = String::from_utf8_lossy(&header).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
                let mut body = vec![0; length]; stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                let response = if step % 2 == 0 {
                    assert_eq!(form["xquery"], DISCOVER);
                    json!({"now":"2026-10-14T09:00:00Z", "hosts":["one"]})
                } else {
                    assert_eq!(form["xquery"], INVENTORY);
                    let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                    assert_eq!(vars["days-ago"], if step == 1 { "0" } else { "-1" });
                    json!({"host":"one", "directory":"/Logs", "inventory":false, "read_bytes":0, "cluster_bytes":0,
                        "files":[{"host":"one","path":"/Logs/ErrorLog.txt","name":"ErrorLog.txt","size":20,"modified":"2026-10-14T09:00:00Z"}], "chunks":[]})
                }.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let mut logs = viewer();
        logs.files.push(file("one"));
        logs.record_history_inventory("one");
        logs.mark_history_checked(logs.now().date());
        logs.history_state = history::HistoryState::default();
        logs.load_history_state(); // Exercise the saved receipt, not just memory.
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        logs.client = Some(client);
        logs.refresh_automatic(&runtime);
        for explicit in [false, true] {
            if explicit {
                logs.refresh(&runtime);
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while logs.busy {
                assert!(Instant::now() < deadline);
                logs.tick(&runtime, false);
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        });
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn settled_archive_is_local_once_and_rotation_reuses_bytes_and_index() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let text = "2026-10-13 08:00:00.000 Info: settled record\n";
        let mut source = named_file("one", "ErrorLog_1.txt", "2026-10-13T12:00:00Z");
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, text, |meta| {
            meta.full_size = Some(source.size);
            meta.end = text.len() as u64;
            meta.length = meta.end;
        })
        .unwrap();
        let before = logs.snapshots[&source.key()].meta();
        let raw = before.content_path(&logs.cache_root);
        let spec = ParseSpec {
            generation: before.generation,
            bytes: before.text_bytes,
            offset: logs.server_offset,
            window: TimeWindow::default(),
            before: None,
            paged: false,
            filter_id: 0,
        };
        let stop = AtomicBool::new(false);
        index::read(
            &raw,
            &before.index_path(&logs.cache_root),
            &source,
            spec,
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        let index_modified = fs::metadata(before.index_path(&logs.cache_root))
            .unwrap()
            .modified()
            .unwrap();
        assert!(logs.endpoints.is_empty());
        logs.tick_archive(&runtime);
        assert!(
            logs.archive_job.is_some(),
            "settled data can be archived with no server connection"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while logs.archive_job.is_some() {
            assert!(Instant::now() < deadline);
            logs.tick_archive(&runtime);
            std::thread::sleep(Duration::from_millis(1));
        }
        let archived = logs.snapshots[&source.key()].meta();
        assert!(archived.archive.is_some());
        assert!(!raw.exists());
        assert_eq!(
            fs::metadata(archived.index_path(&logs.cache_root))
                .unwrap()
                .modified()
                .unwrap(),
            index_modified
        );
        assert!(!logs.archive_dirty);
        let scanned = Instant::now() - STATUS_INTERVAL;
        logs.archive_last_scan = Some(scanned);
        logs.tick_archive(&runtime);
        assert_eq!(
            logs.archive_last_scan,
            Some(scanned),
            "unchanged settled history must not be rescanned"
        );

        let mut target = source.clone();
        target.name = "ErrorLog_2.txt".into();
        target.path = "/Logs/ErrorLog_2.txt".into();
        logs.commit(target.clone(), true, "partial", |_| {})
            .unwrap();
        let promoted = promote_rotated_cache(
            &logs.cache_root,
            &target,
            &archived,
            &stop,
            &AtomicU64::new(0),
        )
        .unwrap();
        assert_eq!(promoted.generation, 1);
        assert!(
            !promoted.content_path(&logs.cache_root).exists(),
            "don't inflate or copy an existing archive"
        );
        let target_archive = promoted.archive_path(&logs.cache_root).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                fs::metadata(&target_archive).unwrap().ino(),
                fs::metadata(archived.archive_path(&logs.cache_root).unwrap())
                    .unwrap()
                    .ino()
            );
        }
        let mut renamed_spec = spec;
        renamed_spec.generation = promoted.generation;
        let (stream, _) = index::read(
            &target_archive,
            &promoted.index_path(&logs.cache_root),
            &target,
            renamed_spec,
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        assert_eq!(stream.records.len(), 1);
        assert_eq!(
            fs::metadata(promoted.index_path(&logs.cache_root))
                .unwrap()
                .modified()
                .unwrap(),
            index_modified,
            "renaming a generation must not rebuild its immutable index"
        );
        assert_eq!(archived.source_identity(), promoted.source_identity());
        logs.files.push(target.clone());
        logs.snapshots.insert(target.key(), promoted.clone());
        logs.restore_retained_archives(None).unwrap();
        assert!(
            logs.retained_archive_days.is_empty(),
            "active archive aliases must not reappear as duplicate retained generations"
        );
        archive::remove(&archived.archive_path(&logs.cache_root).unwrap());
        let mut kept = String::new();
        promoted
            .open_content(&logs.cache_root)
            .unwrap()
            .read_to_string(&mut kept)
            .unwrap();
        assert_eq!(
            kept, text,
            "retiring one alias must not delete another's data"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn settled_unselected_history_loads_metadata_without_reading_log_text() {
        let mut logs = viewer();
        let current = file("one");
        logs.files.push(current.clone());
        logs.commit(current, true, "", |_| {}).unwrap();
        let text = "127.0.0.1 - - [13/Oct/2026:12:00:00 +0000] \"GET /old HTTP/1.1\" 200 1\n";
        let mut old = named_file("one", "8000_AccessLog_1.txt", "2026-10-13T12:00:00Z");
        old.size = text.len() as u64;
        logs.files.push(old.clone());
        logs.commit(old.clone(), true, text, |meta| {
            meta.full_size = Some(old.size);
            meta.end = text.len() as u64;
            meta.length = meta.end;
        })
        .unwrap();
        atomic_write(
            &logs.cache_root.join(CATALOG_FILE),
            &serde_json::to_vec(&logs.files).unwrap(),
        )
        .unwrap();
        let mut restored = viewer();
        fs::remove_dir_all(&restored.cache_root).unwrap();
        restored.cache_root = logs.cache_root.clone();
        restored.load_cache().unwrap();
        let key = old.key();
        assert!(restored.snapshots[&key].text.is_empty());
        assert_eq!(restored.snapshots[&key].text_bytes, text.len() as u64);
        assert!(!restored.parsed_streams.contains_key(&key));
        assert!(!restored.parse_jobs.contains_key(&key));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn historical_check_runs_after_one_am_once_per_server_day() {
        let mut logs = viewer();
        logs.client = Some(test_client());
        logs.endpoints = vec![Endpoint {
            host: "one".into(),
            client: test_client(),
        }];
        logs.inventoried_hosts.insert("one".into());
        logs.tracking_periods.insert(
            "ErrorLog.txt".into(),
            vec![TrackingPeriod {
                start: NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
                end: None,
            }],
        );
        let mut file = named_file("one", "ErrorLog_1.txt", "2026-10-13T08:00:00Z");
        file.size = 4;
        logs.files.push(file.clone());
        logs.commit(file, true, "old\n", |snapshot| snapshot.full_size = Some(4))
            .unwrap();
        let day = NaiveDate::from_ymd_opt(2026, 10, 14).unwrap();
        logs.clock = Some((day.and_hms_opt(0, 59, 0).unwrap(), Instant::now()));
        assert!(!logs.history_due());
        logs.clock = Some((day.and_hms_opt(1, 0, 0).unwrap(), Instant::now()));
        assert!(logs.history_due());
        let runtime = Runtime::new().unwrap();
        logs.begin_activity("Discovering logs", 1);
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&runtime, false);
        assert_eq!(logs.history_checked, Some(day));
        assert!(!logs.history_due());
        logs.clock = Some((
            (day + Days::days(1)).and_hms_opt(0, 59, 0).unwrap(),
            Instant::now(),
        ));
        assert!(!logs.history_due());
        logs.clock = Some((
            (day + Days::days(1)).and_hms_opt(1, 0, 0).unwrap(),
            Instant::now(),
        ));
        assert!(logs.history_due());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn past_file_can_be_tracked_downloaded_and_survive_rotation_without_changing_today() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        let listener = app
            .rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = app.rt.spawn(async move {
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
            let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["path"], "/Logs/9000_ErrorLog_3.txt");
            assert_eq!(requests[0]["full"], true);
            let reply = json!({"host":"one", "directory":"/Logs", "inventory":false,
                "read_bytes":4, "cluster_bytes":0,
                "files":[{"host":"one", "path":"/Logs/9000_ErrorLog_3.txt", "name":"9000_ErrorLog_3.txt", "size":4, "modified":"2026-10-11T08:00:00Z"}],
                "chunks":[{"host":"one", "path":"/Logs/9000_ErrorLog_3.txt", "start":0, "end":4, "length":4, "reset":true, "text":"old\n", "full":true}]}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}", reply.len()).as_bytes()).await.unwrap();
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        let today = named_file("one", "9000_ErrorLog.txt", "2026-10-14T09:00:00Z");
        let yesterday = named_file("one", "9000_ErrorLog_1.txt", "2026-10-13T09:00:00Z");
        let mut old = named_file("one", "9000_ErrorLog_3.txt", "2026-10-11T08:00:00Z");
        let other = named_file("one", "9001_AccessLog.txt", "2026-10-14T09:00:00Z");
        old.size = 4;
        logs.files = vec![
            today.clone(),
            yesterday.clone(),
            old.clone(),
            other.clone(),
            named_file("one", "9000_AccessLog_3.txt", "2026-10-11T08:00:00Z"),
        ];
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.set_selected_mode(today.clone(), TrackingMode::Live);
        logs.set_selected_mode(other.clone(), TrackingMode::OnDemand);
        logs.select_grid_cell("9000", 3, 0);
        app.server_logs = logs;
        let send = |app: &mut crate::App, ch| {
            crate::events::handle_key_event(
                app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            )
            .unwrap();
        };
        assert_eq!(app.server_logs.tracking_mode(&old), TrackingMode::Untracked);
        send(&mut app, 'o');
        assert_eq!(app.server_logs.tracking_mode(&old), TrackingMode::OnDemand);
        assert_eq!(app.server_logs.tracking_mode(&today), TrackingMode::Live);
        assert_eq!(
            app.server_logs.tracking_mode(&other),
            TrackingMode::OnDemand
        );
        assert_eq!(
            app.server_logs.tracking_mode(&yesterday),
            TrackingMode::Untracked
        );
        assert!(
            app.server_logs
                .plan(None)
                .iter()
                .flat_map(|plan| &plan.timed)
                .all(|request| request.name != old.name)
        );
        app.server_logs.combined = true;
        app.server_logs.hosts_focus = true;
        send(&mut app, 'd');
        assert!(app.server_logs.busy);
        assert_eq!(app.server_logs.activity.as_ref().unwrap().total, 1);
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while app.server_logs.busy {
                    app.server_logs.tick(&app.rt, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        app.rt.block_on(server).unwrap();
        assert_eq!(app.server_logs.snapshots[&old.key()].text, "old\n");
        assert_eq!(app.server_logs.snapshots[&old.key()].full_size, Some(4));
        send(&mut app, 'o');
        assert_eq!(app.server_logs.tracking_mode(&old), TrackingMode::Untracked);
        assert_eq!(app.server_logs.tracking_mode(&today), TrackingMode::Live);
        send(&mut app, 't');
        assert_eq!(app.server_logs.tracking_mode(&old), TrackingMode::OnDemand);
        let mut restored = LogViewer {
            cache_root: app.server_logs.cache_root.clone(),
            clock: app.server_logs.clock,
            ..LogViewer::default()
        };
        restored.load_subscriptions();
        assert_eq!(restored.tracking_mode(&old), TrackingMode::OnDemand);
        assert_eq!(restored.tracking_mode(&today), TrackingMode::Live);
        restored.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 15)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        let rotated = named_file("one", "9000_ErrorLog_4.txt", "2026-10-11T08:00:00Z");
        assert_eq!(restored.tracking_mode(&rotated), TrackingMode::OnDemand);
        assert_eq!(restored.tracking_mode(&old), TrackingMode::Untracked);
        assert_eq!(restored.tracking_mode(&today), TrackingMode::Live);
        assert_eq!(restored.tracking_mode(&yesterday), TrackingMode::OnDemand);
        restored.set_selected_mode(today.clone(), TrackingMode::Untracked);
        assert_eq!(restored.tracking_mode(&rotated), TrackingMode::OnDemand);
        assert_eq!(restored.tracking_mode(&today), TrackingMode::Untracked);
        assert_eq!(restored.tracking_mode(&other), TrackingMode::OnDemand);
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn legacy_individual_rotation_settings_do_not_change_the_current_stream() {
        let mut logs = viewer();
        fs::write(logs.cache_root.join(SUBSCRIPTIONS_FILE), serde_json::to_vec(&json!({
            "modes": {"9000_ErrorLog.txt":"partial", "9000_ErrorLog_3.txt":"live", "9000_ErrorLog_2.txt":"untracked"}
        })).unwrap()).unwrap();
        logs.load_subscriptions();
        let file = |name| named_file("one", name, "2026-10-14T09:00:00Z");
        assert_eq!(
            logs.tracking_mode(&file("9000_ErrorLog.txt")),
            TrackingMode::Partial
        );
        assert_eq!(
            logs.tracking_mode(&file("9000_ErrorLog_3.txt")),
            TrackingMode::OnDemand
        );
        assert_eq!(
            logs.tracking_mode(&file("9000_ErrorLog_2.txt")),
            TrackingMode::Untracked
        );
        logs.save_subscriptions();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            clock: logs.clock,
            ..LogViewer::default()
        };
        restored.load_subscriptions();
        assert_eq!(
            restored.tracking_mode(&file("9000_ErrorLog.txt")),
            TrackingMode::Partial
        );
        assert_eq!(
            restored.tracking_mode(&file("9000_ErrorLog_3.txt")),
            TrackingMode::OnDemand
        );
        assert_eq!(
            restored.tracking_mode(&file("9000_ErrorLog_2.txt")),
            TrackingMode::Untracked
        );
        restored.files = vec![
            file("9000_ErrorLog.txt"),
            file("9000_ErrorLog_3.txt"),
            file("9000_ErrorLog_2.txt"),
        ];
        restored.prune_untracked_inventory();
        assert!(
            restored
                .files
                .iter()
                .any(|file| file.name == "9000_ErrorLog_3.txt")
        );
        assert!(
            !restored
                .files
                .iter()
                .any(|file| file.name == "9000_ErrorLog_2.txt")
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn tracking_periods_follow_the_stream_and_survive_restart() {
        let mut logs = viewer();
        let mut current = named_file("one", "9000_ErrorLog.txt", "2026-10-14T09:00:00Z");
        logs.files.push(current.clone());
        logs.set_selected_mode(current.clone(), TrackingMode::OnDemand);
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 16)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        current.name = "9000_ErrorLog_1.txt".into();
        assert_eq!(logs.tracking_mode(&current), TrackingMode::OnDemand);
        current.name = "9000_ErrorLog_3.txt".into();
        assert_eq!(logs.tracking_mode(&current), TrackingMode::Untracked);
        logs.set_selected_mode(current.clone(), TrackingMode::Untracked);
        current.name = "9000_ErrorLog.txt".into();
        assert_eq!(logs.tracking_mode(&current), TrackingMode::OnDemand);
        logs.set_selected_mode(current.clone(), TrackingMode::Untracked);
        logs.clock = Some((
            NaiveDate::from_ymd_opt(2026, 10, 18)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
            Instant::now(),
        ));
        current.name = "9000_ErrorLog_2.txt".into();
        assert_eq!(logs.tracking_mode(&current), TrackingMode::OnDemand);
        current.name = "9000_ErrorLog_1.txt".into();
        assert_eq!(logs.tracking_mode(&current), TrackingMode::Untracked);
        logs.set_selected_mode(current.clone(), TrackingMode::OnDemand);
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            clock: logs.clock,
            ..LogViewer::default()
        };
        restored.load_subscriptions();
        current.name = "9000_ErrorLog_1.txt".into();
        assert_eq!(restored.tracking_mode(&current), TrackingMode::OnDemand);
        current.name = "9000_ErrorLog.txt".into();
        assert_eq!(restored.tracking_mode(&current), TrackingMode::Untracked);
        current.name = "9000_ErrorLog_2.txt".into();
        assert_eq!(restored.tracking_mode(&current), TrackingMode::OnDemand);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn historical_work_skips_complete_compressed_caches_and_untracked_days() {
        let mut logs = viewer();
        logs.endpoints = vec![Endpoint {
            host: "one".into(),
            client: test_client(),
        }];
        logs.tracking_periods.insert(
            "ErrorLog.txt".into(),
            vec![TrackingPeriod {
                start: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
                end: None,
            }],
        );
        for name in ["ErrorLog_1.txt", "ErrorLog_2.txt", "ErrorLog_3.txt"] {
            let mut entry = named_file("one", name, "2026-10-13T08:00:00Z");
            entry.size = 4;
            logs.files.push(entry);
        }
        let complete = logs.files[0].clone();
        logs.commit(complete.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4)
        })
        .unwrap();
        let raw = logs.snapshots[&complete.key()].content_path(&logs.cache_root);
        let dir = logs.cache_root.join("archives");
        fs::create_dir_all(&dir).unwrap();
        archive::build(&raw, &dir.join("old.gz"), 4, &AtomicBool::new(false)).unwrap();
        logs.publish_archive(&complete.key(), 0, 4, "old.gz".into())
            .unwrap();
        fs::remove_file(raw).unwrap();
        let (copies, plans) = logs.historical_work();
        assert!(copies.is_empty());
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].1.len(), 1);
        assert_eq!(plans[0].1[0]["path"], "/Logs/ErrorLog_2.txt");
        logs.files[0].size = 5;
        let (_, changed) = logs.historical_work();
        assert_eq!(
            changed[0].1.len(),
            2,
            "a changed remote size must be downloaded again"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn rotated_complete_cache_is_preserved_without_another_download() {
        let mut logs = viewer();
        logs.endpoints = vec![Endpoint {
            host: "one".into(),
            client: test_client(),
        }];
        logs.tracking_periods.insert(
            "ErrorLog.txt".into(),
            vec![TrackingPeriod {
                start: NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
                end: None,
            }],
        );
        let mut source = named_file("one", "ErrorLog.txt", "2026-10-13T08:00:00Z");
        source.size = 4;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4)
        })
        .unwrap();
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let dir = logs.cache_root.join("archives");
        fs::create_dir_all(&dir).unwrap();
        archive::build(&raw, &dir.join("source.gz"), 4, &AtomicBool::new(false)).unwrap();
        logs.publish_archive(&source.key(), 0, 4, "source.gz".into())
            .unwrap();
        fs::remove_file(raw).unwrap();
        logs.files[0].modified = "2026-10-14T08:00:00Z".into();
        let mut rotated = named_file("one", "ErrorLog_1.txt", "2026-10-13T08:00:00Z");
        rotated.size = 4;
        logs.files.push(rotated.clone());
        let (copies, plans) = logs.historical_work();
        assert_eq!(copies.len(), 1);
        assert!(plans.is_empty());
        let progress = AtomicU64::new(0);
        let snapshot = promote_rotated_cache(
            &logs.cache_root,
            &copies[0].0,
            &copies[0].1,
            &AtomicBool::new(false),
            &progress,
        )
        .unwrap();
        assert_eq!(progress.load(Ordering::Relaxed), 4);
        logs.snapshots.insert(rotated.key(), snapshot);
        logs.commit(source, true, "new\n", |_| {}).unwrap();
        let mut content = String::new();
        logs.snapshots[&rotated.key()]
            .open_content(&logs.cache_root)
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert_eq!(content, "old\n");
        assert!(logs.historical_work().0.is_empty());
        assert!(logs.historical_work().1.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn rotated_cache_replaces_same_size_copy_from_a_different_day() {
        for (old_text, new_text) in [("old\n", "new\n"), ("", "")] {
            let mut logs = viewer();
            let mut source = named_file("one", "ErrorLog.txt", "2026-10-13T08:00:00Z");
            source.size = new_text.len() as u64;
            let mut target = named_file("one", "ErrorLog_1.txt", "2026-10-12T08:00:00Z");
            target.size = old_text.len() as u64;
            logs.commit(source.clone(), true, new_text, |meta| {
                meta.full_size = Some(source.size)
            })
            .unwrap();
            logs.commit(target.clone(), true, old_text, |meta| {
                meta.full_size = Some(target.size)
            })
            .unwrap();
            target.modified = source.modified.clone();
            let progress = AtomicU64::new(0);
            let promoted = promote_rotated_cache(
                &logs.cache_root,
                &target,
                &logs.snapshots[&source.key()],
                &AtomicBool::new(false),
                &progress,
            )
            .unwrap();
            assert_eq!(promoted.file.modified, target.modified);
            let mut content = String::new();
            promoted
                .open_content(&logs.cache_root)
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, new_text);
            assert_eq!(progress.load(Ordering::Relaxed), source.size);
            // A second check reuses the correct copy without writing it again.
            let repeated = promote_rotated_cache(
                &logs.cache_root,
                &target,
                &logs.snapshots[&source.key()],
                &AtomicBool::new(false),
                &progress,
            )
            .unwrap();
            assert_eq!(repeated.generation, promoted.generation);
            assert_eq!(progress.load(Ordering::Relaxed), source.size);
            fs::remove_dir_all(logs.cache_root).unwrap();
        }
    }

    #[test]
    fn rotated_cache_chain_preserves_older_sources_and_finishes() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        logs.client = Some(test_client());
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client: test_client(),
        });
        logs.inventoried_hosts.insert("one".into());
        logs.tracking_periods.insert(
            "ErrorLog.txt".into(),
            vec![TrackingPeriod {
                start: NaiveDate::from_ymd_opt(2026, 10, 12).unwrap(),
                end: None,
            }],
        );
        for (name, modified, text) in [
            ("ErrorLog.txt", "2026-10-13T08:00:00Z", "new\n"),
            ("ErrorLog_1.txt", "2026-10-12T08:00:00Z", "old\n"),
        ] {
            let mut file = named_file("one", name, modified);
            file.size = text.len() as u64;
            logs.files.push(file.clone());
            logs.commit(file, true, text, |meta| meta.full_size = Some(4))
                .unwrap();
        }
        logs.files[0].modified = "2026-10-14T08:00:00Z".into();
        logs.files[1].modified = "2026-10-13T08:00:00Z".into();
        let mut older = named_file("one", "ErrorLog_2.txt", "2026-10-12T08:00:00Z");
        older.size = 4;
        logs.files.push(older);
        logs.start_historical_catchup(&runtime);
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(2), async {
                while logs.busy {
                    logs.tick(&runtime, false);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
        });
        assert!(logs.errors.is_empty(), "{:?}", logs.errors);
        assert_eq!(logs.history_checked, Some(logs.now().date()));
        assert!(!logs.history_due());
        let (copies, plans) = logs.historical_work();
        assert!(copies.is_empty());
        assert!(plans.is_empty());
        for (file, expected) in [(&logs.files[1], "new\n"), (&logs.files[2], "old\n")] {
            let snapshot = &logs.snapshots[&file.key()];
            assert_eq!(snapshot.file.modified, file.modified);
            let mut content = String::new();
            snapshot
                .open_content(&logs.cache_root)
                .unwrap()
                .read_to_string(&mut content)
                .unwrap();
            assert_eq!(content, expected);
        }
        logs.tick(&runtime, false);
        assert!(!logs.busy);
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn calendar_day_reads_cached_records_across_rotations() {
        let mut logs = viewer_on("2026-10-30");
        let text =
            "2026-10-29 17:00:00.000 Info: cached yesterday\n2026-10-30 10:00:00.000 Info: today\n";
        let mut current = named_file("one", "TaskServer_ErrorLog.txt", "2026-10-30T10:00:00Z");
        current.size = text.len() as u64;
        logs.files.push(current.clone());
        logs.commit(current, true, text, |_| {}).unwrap();
        logs.select_grid_cell("TASK", 1, 0);
        assert!(logs.visible_text().contains("cached yesterday"));
        assert!(!logs.visible_text().contains("Info: today"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn rotations_share_one_pane_per_host_and_empty_rotations_add_no_panes() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut logs = viewer();
        let day = logs.now().date();
        logs.window = TimeWindow {
            start: day.and_hms_opt(8, 0, 0),
            end: day.and_hms_opt(9, 0, 0),
        };
        for host in 1..=9 {
            for rotation in 0..7 {
                let name = if rotation == 0 {
                    "ErrorLog.txt".into()
                } else {
                    format!("ErrorLog_{rotation}.txt")
                };
                let text = match rotation {
                    0 => {
                        "2026-10-14 08:02:00.000 Info: current-other\n2026-10-14 08:04:00.000 Info: current-tail\n"
                    }
                    1 => {
                        "2026-10-14 08:01:00.000 Info: rotationhit\n2026-10-14 08:03:00.000 Info: rotation-context\n"
                    }
                    _ => "2026-10-13 08:00:00.000 Info: old-rotation\n",
                };
                let modified = if rotation == 0 {
                    "2026-10-14T12:00:00Z"
                } else {
                    "2026-10-13T12:00:00Z"
                };
                let mut file = named_file(&format!("host-{host:02}"), &name, modified);
                file.size = text.len() as u64;
                logs.files.push(file.clone());
                logs.commit(file, true, text, |_| {}).unwrap();
            }
        }
        logs.select(0);
        assert_eq!(logs.host_views.len(), 9);
        assert_eq!(
            logs.host_views
                .iter()
                .map(|view| &view.host)
                .collect::<BTreeSet<_>>()
                .len(),
            9
        );
        for view in &logs.host_views {
            assert_eq!(view.keys.len(), 2, "only rotations with records contribute");
            assert_eq!(
                view.rows.iter().filter(|row| row.truncation_marker).count(),
                2
            );
            assert_eq!(view.rows.iter().filter(|row| row.start).count(), 4);
            let times: Vec<_> = view.rows.iter().filter_map(|row| row.timestamp).collect();
            assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        logs.focus_host(1);
        assert_eq!(logs.selected_file().unwrap().host, "host-02");
        let mut terminal = Terminal::new(TestBackend::new(160, 120)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        assert_eq!(
            logs.mouse_regions
                .iter()
                .filter(|(_, target)| matches!(target, LogMouseTarget::Host(_)))
                .count(),
            9
        );

        logs.search = "rotationhit".into();
        logs.filter();
        for view in &logs.host_views {
            assert_eq!(
                view.matches
                    .iter()
                    .filter(|&&row| !view.rows[row].truncation_marker)
                    .count(),
                1
            );
        }
        logs.context_after = 1;
        logs.filter();
        for view in &logs.host_views {
            let text = view
                .matches
                .iter()
                .map(|&row| logs.display_host_row(view, row).to_string())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("rotationhit") && text.contains("rotation-context"));
            assert!(
                !text.contains("current-other"),
                "context must stay within its source file"
            );
        }
        logs.search.clear();
        logs.context_after = 0;
        let rotation = logs
            .files
            .iter()
            .find(|file| file.host == "host-02" && file.name == "ErrorLog_1.txt")
            .unwrap()
            .clone();
        let key = rotation.key();
        let old_len = logs
            .commit(
                rotation,
                false,
                "2026-10-14 08:05:00.000 Info: late-rotation-record\n",
                |_| {},
            )
            .unwrap();
        logs.extend_view(&key, false, old_len);
        assert_eq!(logs.host_views.len(), 9);
        assert!(
            logs.visible_text().contains("late-rotation-record"),
            "updates to another rotation must refresh the host pane"
        );
        logs.window.start = day.and_hms_opt(18, 0, 0);
        logs.window.end = day.and_hms_opt(19, 0, 0);
        logs.rebuild_lines();
        assert!(
            logs.host_views.is_empty(),
            "truncation markers alone must never create panes"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn retained_archive_generations_appear_on_their_actual_day_after_restart() {
        let mut logs = viewer_on("2026-11-01");
        let source = named_file("one", "7997_AccessLog_6.txt", "2026-10-25T23:59:59Z");
        let old =
            "127.0.0.1 - - [25/Oct/2026:12:00:00 +0000] \"GET /old-local-record HTTP/1.1\" 200 1\n";
        let new = "127.0.0.1 - - [26/Oct/2026:12:00:00 +0000] \"GET /new-record HTTP/1.1\" 200 1\n";
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, old, |_| {}).unwrap();
        let archive_dir = logs.cache_root.join("archives");
        fs::create_dir_all(&archive_dir).unwrap();
        let archive_path = archive_dir.join(format!("{}-0-retained.gz", hash_key(&source.key())));
        archive::build(
            &logs.snapshots[&source.key()].content_path(&logs.cache_root),
            &archive_path,
            old.len() as u64,
            &AtomicBool::new(false),
        )
        .unwrap();
        logs.commit(source.clone(), true, new, |_| {}).unwrap();
        let mut restored = viewer_on("2026-11-01");
        fs::remove_dir_all(&restored.cache_root).unwrap();
        restored.cache_root = logs.cache_root.clone();
        restored.load_cache().unwrap();
        restored.reset_time_tracking();
        let row = restored
            .port_rows_for_day(7)
            .into_iter()
            .find(|(port, _)| port == "7997")
            .unwrap();
        assert!(
            row.1[1].is_some(),
            "retained data must be marked in the grid before selection"
        );
        restored.select_grid_cell("7997", 7, 1);
        let runtime = Runtime::new().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !restored.parse_jobs.is_empty() {
            assert!(Instant::now() < deadline);
            restored.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            restored.visible_text().contains("/old-local-record"),
            "view={:?} window={:?} selected={:?} hostviews={} snapshots={} rows={}",
            restored.visible_text(),
            restored.display_window(),
            restored.selected_file().map(|f| &f.path),
            restored.host_views.len(),
            restored.snapshots.len(),
            restored.rows.len()
        );
        assert!(!restored.visible_text().contains("/new-record"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn historical_cached_day_ignores_only_the_automatic_recent_window() {
        let mut logs = viewer_on("2026-11-01");
        let text = "2026-10-25 17:00:00.000 Info: retained week-old record\n";
        let mut file = named_file("one", "TaskServer_ErrorLog_6.txt", "2026-10-25T18:00:00Z");
        file.size = text.len() as u64;
        file.present = false;
        logs.files.push(file.clone());
        logs.commit(file, true, text, |_| {}).unwrap();
        logs.reset_time_tracking();
        logs.select_grid_cell("TASK", 6, 0);
        logs.select_grid_horizontal(true, false);
        assert_eq!(logs.discovery_days_ago, 7);
        assert!(logs.visible_text().contains("retained week-old record"));
        logs.automatic_window = false;
        logs.rebuild_lines();
        assert!(!logs.visible_text().contains("retained week-old record"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn cached_records_remain_visible_after_remote_file_is_truncated() {
        let mut logs = viewer_on("2026-10-30");
        let text = "2026-10-29 17:00:00.000 Info: retained local record\n";
        let mut source = named_file("one", "TaskServer_ErrorLog.txt", "2026-10-29T17:00:00Z");
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source, true, text, |_| {}).unwrap();
        logs.files[0].size = 0;
        logs.files[0].modified = "2026-10-30T12:00:00Z".into();
        logs.select_grid_cell("TASK", 1, 0);
        assert!(logs.visible_text().contains("retained local record"));
        assert_eq!(
            logs.grid_freshness(Utc::now())["TaskServer_ErrorLog.txt"],
            CacheFreshness::Stale
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn manual_download_rejects_empty_and_absent_files() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut empty = named_file("one", "ErrorLog.txt", "2026-10-30T10:00:00Z");
        empty.size = 0;
        logs.files.push(empty);
        logs.download(&runtime);
        assert!(logs.message.contains("No non-empty"));
        assert!(logs.manual_downloads_queued.is_empty());
        logs.files[0].size = 10;
        logs.files[0].present = false;
        logs.download_now(&runtime);
        assert!(logs.message.contains("No non-empty"));
        assert!(logs.manual_downloads_queued.is_empty());
    }

    #[test]
    fn rotation_matches_yesterdays_cached_current_file_and_opens_it() {
        let mut logs = viewer_on("2026-10-30");
        let text = "2026-10-29 17:00:00.000 Info: cached yesterday\n";
        let mut old = named_file("one", "TaskServer_ErrorLog.txt", "2026-10-29T17:00:00Z");
        old.size = text.len() as u64;
        logs.files.push(old.clone());
        logs.commit(old, true, text, |snapshot| {
            snapshot.full_size = Some(text.len() as u64)
        })
        .unwrap();
        logs.files[0].modified = "2026-10-30T09:00:00Z".into();
        logs.files[0].size = 100;
        let mut rotated = named_file("one", "TaskServer_ErrorLog_1.txt", "2026-10-29T18:00:00Z");
        rotated.size = text.len() as u64 + 20;
        logs.files.push(rotated);
        let statuses = logs.grid_freshness(Utc::now());
        assert_eq!(statuses["TaskServer_ErrorLog.txt"], CacheFreshness::Missing);
        assert_eq!(statuses["TaskServer_ErrorLog_1.txt"], CacheFreshness::Stale);
        assert_eq!(
            logs.grid_tracking_modes()["TaskServer_ErrorLog_1.txt"],
            TrackingMode::OnDemand
        );
        logs.select_grid_cell("TASK", 1, 0);
        assert_eq!(
            logs.selected_file().unwrap().name,
            "TaskServer_ErrorLog_1.txt"
        );
        assert!(logs.visible_text().contains("cached yesterday"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn grid_uses_the_least_fresh_available_host_copy() {
        let mut logs = viewer();
        let now = Utc::now();
        for host in ["one", "two"] {
            let mut file = named_file(host, "TaskServer_ErrorLog_1.txt", "2026-10-14T09:00:00Z");
            file.size = 4;
            logs.files.push(file.clone());
            if host == "one" {
                logs.commit(file, true, "test", |snapshot| snapshot.full_size = Some(4))
                    .unwrap();
            }
        }
        assert_eq!(
            logs.grid_freshness(now)["TaskServer_ErrorLog_1.txt"],
            CacheFreshness::Stale
        );
        logs.files[1].present = false;
        assert_eq!(
            logs.grid_freshness(now)["TaskServer_ErrorLog_1.txt"],
            CacheFreshness::Fresh
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn grid_freshness_ignores_empty_host_copies_but_reports_missing_content() {
        let mut logs = viewer();
        let now = Utc::now();
        for name in ["9000_ErrorLog.txt", "9000_ErrorLog_1.txt"] {
            let mut downloaded = named_file("one", name, "2026-10-14T09:00:00Z");
            downloaded.size = 4;
            let mut empty = named_file("two", name, "2026-10-14T09:00:00Z");
            empty.size = 0;
            let mut absent = named_file("three", name, "2026-10-14T09:00:00Z");
            absent.present = false;
            logs.files.extend([downloaded.clone(), empty, absent]);
            logs.commit(downloaded, true, "test", |snapshot| {
                snapshot.full_size = Some(4)
            })
            .unwrap();
            assert_eq!(logs.grid_freshness(now)[name], CacheFreshness::Fresh);
            let empty = logs
                .files
                .iter_mut()
                .find(|file| file.host == "two" && file.name == name)
                .unwrap();
            empty.size = 4;
            assert_eq!(logs.grid_freshness(now)[name], CacheFreshness::Stale);
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn complete_rotated_cache_stays_fresh_when_only_modification_time_changes() {
        let mut logs = viewer();
        let now = Utc::now();
        let mut rotated = named_file("one", "9000_ErrorLog_1.txt", "2026-10-13T08:00:00Z");
        rotated.size = 4;
        logs.files.push(rotated.clone());
        logs.commit(rotated, true, "test", |snapshot| {
            snapshot.full_size = Some(4)
        })
        .unwrap();
        logs.files[0].modified = "2026-10-13T09:00:00Z".into();
        assert_eq!(
            logs.grid_freshness(now)["9000_ErrorLog_1.txt"],
            CacheFreshness::Fresh
        );
        logs.files[0].size = 5;
        assert_eq!(
            logs.grid_freshness(now)["9000_ErrorLog_1.txt"],
            CacheFreshness::Stale
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn complete_historical_cache_stays_green_when_inventory_is_unavailable() {
        let mut logs = viewer();
        let mut source = named_file("one", "ErrorLog_1.txt", "2026-10-14T00:00:00Z");
        source.size = 4;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        logs.snapshots.get_mut(&source.key()).unwrap().updated_at =
            Some("2026-10-14T01:00:00Z".into());
        assert!(!logs.snapshots[&source.key()].settled(logs.server_offset));
        logs.files[0].present = false;
        logs.note_service_failure("Appserver Disabled (HTTP 503)");
        assert!(logs.inventoried_hosts.is_empty());
        assert_eq!(
            logs.grid_freshness(Utc::now())["ErrorLog_1.txt"],
            CacheFreshness::Fresh
        );
        logs.files[0].size = 5;
        assert_eq!(
            logs.grid_freshness(Utc::now())["ErrorLog_1.txt"],
            CacheFreshness::Stale,
            "known changes still mean the saved copy is outdated"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn disabled_download_preserves_cache_pauses_polling_and_can_be_retried() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        let listener = app
            .rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = app.rt.spawn(async move {
            for step in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") { header.push(stream.read_u8().await.unwrap()); }
                let length: usize = String::from_utf8_lossy(&header).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
                let mut body = vec![0; length]; stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(form["xquery"], COLLECT, "no automatic polls should slip between the explicit attempts");
                let (status, body) = if step == 0 {
                    ("503 Appserver Disabled", json!({"errorResponse":{"statusCode":503,"status":"Appserver Disabled","message":"503 Appserver Disabled"}}).to_string())
                } else {
                    ("200 OK", json!({"host":"one","directory":"/Logs","inventory":false,"read_bytes":4,"cluster_bytes":0,
                        "files":[{"host":"one","path":"/Logs/ErrorLog_1.txt","name":"ErrorLog_1.txt","size":4,"modified":"2026-10-13T12:00:00Z"}],
                        "chunks":[{"host":"one","path":"/Logs/ErrorLog_1.txt","start":0,"end":4,"length":4,"reset":true,"full":true,"text":"new\n"}]}).to_string())
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            }
        });
        let mut logs = viewer();
        let mut old = named_file("one", "ErrorLog_1.txt", "2026-10-13T12:00:00Z");
        old.size = 4;
        logs.files = vec![old.clone(), file("one")];
        logs.discovery_days_ago = 1;
        logs.modes.insert("ErrorLog.txt".into(), TrackingMode::Live);
        logs.commit(old.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        let original = logs.snapshots[&old.key()].content_path(&logs.cache_root);
        let metadata_path = logs
            .cache_root
            .join(format!("{}.json", hash_key(&old.key())));
        let metadata = fs::read(&metadata_path).unwrap();
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        logs.client = Some(client.clone());
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.history_checked = Some(logs.now().date());
        logs.select(0);
        logs.download(&app.rt);
        let deadline = Instant::now() + Duration::from_secs(5);
        while logs.busy {
            assert!(Instant::now() < deadline);
            logs.tick(&app.rt, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!logs.automatic_collection_allowed());
        assert!(
            logs.service_status()
                .unwrap()
                .contains("AppServer disabled")
        );
        assert_eq!(fs::read(&metadata_path).unwrap(), metadata);
        assert_eq!(fs::read_to_string(&original).unwrap(), "old\n");
        assert_eq!(
            logs.grid_freshness(Utc::now())["ErrorLog_1.txt"],
            CacheFreshness::Fresh
        );
        for _ in 0..10 {
            logs.last_poll = Instant::now() - INTERVAL;
            logs.tick(&app.rt, true);
            assert!(!logs.busy);
        }
        app.server_logs = logs;
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(app.server_logs.errors_open);
        let mut terminal = Terminal::new(TestBackend::new(140, 32)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Log errors") && screen.contains("AppServer disabled"));
        assert!(screen.contains("HTTP 503"));
        crate::events::handle_server_logs_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .unwrap();
        app.server_logs.download(&app.rt);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.server_logs.busy {
            assert!(Instant::now() < deadline);
            app.server_logs.tick(&app.rt, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(app.server_logs.automatic_collection_allowed());
        assert_eq!(app.server_logs.snapshots[&old.key()].text, "new\n");
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap();
        });
        app.server_logs.cancel();
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn grid_cache_freshness_covers_current_and_historical_copies() {
        let mut logs = viewer();
        let now = Utc::now();
        for name in [
            "ErrorLog.txt",
            "TaskServer_ErrorLog_1.txt",
            "8000_ErrorLog_2.txt",
        ] {
            let mut file = named_file("one", name, "2026-10-14T09:00:00Z");
            file.size = 4;
            logs.files.push(file.clone());
            logs.commit(file, true, "test", |snapshot| snapshot.full_size = Some(4))
                .unwrap();
        }
        let state = |logs: &LogViewer, name: &str| logs.grid_freshness(now)[name];
        assert_eq!(state(&logs, "ErrorLog.txt"), CacheFreshness::Fresh);
        let current = logs.files[0].key();
        logs.snapshots.get_mut(&current).unwrap().updated_at =
            Some((now - Days::minutes(30)).to_rfc3339());
        assert_eq!(state(&logs, "ErrorLog.txt"), CacheFreshness::Stale);
        logs.snapshots.get_mut(&current).unwrap().text_bytes = 0;
        assert_eq!(state(&logs, "ErrorLog.txt"), CacheFreshness::Missing);
        assert_eq!(
            state(&logs, "TaskServer_ErrorLog_1.txt"),
            CacheFreshness::Fresh
        );
        let historical = logs.files[1].key();
        logs.snapshots.get_mut(&historical).unwrap().full_size = None;
        assert_eq!(
            state(&logs, "TaskServer_ErrorLog_1.txt"),
            CacheFreshness::Stale
        );
        logs.files[1].present = false;
        logs.inventoried_hosts.insert("one".into());
        assert_eq!(
            state(&logs, "TaskServer_ErrorLog_1.txt"),
            CacheFreshness::Fresh
        );
        assert_eq!(
            logs.grid_tracking_modes()["TaskServer_ErrorLog_1.txt"],
            TrackingMode::OnDemand
        );
        logs.inventoried_hosts.clear();
        assert_eq!(
            state(&logs, "TaskServer_ErrorLog_1.txt"),
            CacheFreshness::Stale
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn only_three_streams_are_tracked_by_default_and_the_rest_are_opt_in() {
        let mut logs = viewer();
        logs.initialize_tracking_periods();
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
        assert!(app.server_logs.empty_grid_cell());
        assert_eq!(app.server_logs.selected_port(), Some("8001"));
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
        assert!(app.server_logs.empty_grid_cell());
        assert_eq!(app.server_logs.selected_port(), Some("8004"));
        send(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "8005_ErrorLog.txt"
        );
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn shift_arrows_reach_empty_cells_without_changing_tracking() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        for name in [
            "8000_ErrorLog.txt",
            "8000_RequestLog_3.txt",
            "9000_AccessLog.txt",
        ] {
            app.server_logs
                .files
                .push(named_file("one", name, "2026-10-14T12:00:00Z"));
            app.server_logs
                .modes
                .insert(name.into(), TrackingMode::Untracked);
        }
        app.server_logs.files_focus = true;
        app.server_logs.select(0);
        app.server_logs.hosts_focus = true;
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
        };
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        send(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert!(!app.server_logs.hosts_focus);
        assert!(app.server_logs.selected_file().is_none());
        assert_eq!(app.server_logs.selected_port(), Some("8000"));
        assert_eq!(app.server_logs.selected_kind_column(), 1);
        for key in ['t', 'o', 'd', 'b', 'D'] {
            send(&mut app, KeyCode::Char(key), KeyModifiers::NONE);
            assert_eq!(app.server_logs.message, "No log is available in this cell.");
        }
        assert!(
            app.server_logs
                .modes
                .values()
                .all(|mode| *mode == TrackingMode::Untracked)
        );
        send(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(
            app.server_logs.selected_file().unwrap().name,
            "9000_AccessLog.txt"
        );
        send(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.server_logs.empty_grid_cell());
        send(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        send(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        assert_eq!(app.server_logs.discovery_days_ago, 1);
        assert_eq!(app.server_logs.selected_kind_column(), 0);
        assert!(app.server_logs.empty_grid_cell());
        send(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        assert_eq!(app.server_logs.discovery_days_ago, 0);
        assert_eq!(app.server_logs.selected_kind_column(), 2);
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
    fn files_t_toggles_only_selected_stream_and_cached_file_shows_untracked() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut logs = viewer();
        let selected = named_file("one", "9000_AccessLog.txt", "2026-10-14T09:00:00Z");
        let second_host = named_file("two", "9000_AccessLog.txt", "2026-10-14T09:00:00Z");
        let other_kind = named_file("one", "9000_ErrorLog.txt", "2026-10-14T09:00:00Z");
        let other_port = named_file("one", "9001_AccessLog.txt", "2026-10-14T09:00:00Z");
        logs.files = vec![
            selected.clone(),
            second_host.clone(),
            other_kind.clone(),
            other_port.clone(),
        ];
        logs.commit(selected.clone(), true, "cached\n", |snapshot| {
            snapshot.full_size = Some(7)
        })
        .unwrap();
        logs.select_grid_cell("9000", 0, 1);
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let send = |app: &mut crate::App, ch| {
            crate::events::handle_key_event(
                app,
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
            )
            .unwrap();
        };
        assert_eq!(
            app.server_logs.grid_tracking_modes()[selected.name.as_str()],
            TrackingMode::Untracked
        );
        send(&mut app, 't');
        assert_eq!(
            app.server_logs.tracking_mode(&selected),
            TrackingMode::OnDemand
        );
        assert_eq!(
            app.server_logs.tracking_mode(&second_host),
            TrackingMode::OnDemand
        );
        assert_eq!(
            app.server_logs.tracking_mode(&other_kind),
            TrackingMode::Untracked
        );
        assert_eq!(
            app.server_logs.tracking_mode(&other_port),
            TrackingMode::Untracked
        );
        assert_eq!(
            app.server_logs.grid_tracking_modes()[selected.name.as_str()],
            TrackingMode::OnDemand
        );
        send(&mut app, 'o');
        assert_eq!(app.server_logs.tracking_mode(&selected), TrackingMode::Live);
        send(&mut app, 't');
        assert_eq!(
            app.server_logs.tracking_mode(&selected),
            TrackingMode::Untracked
        );
        assert_eq!(
            app.server_logs.grid_tracking_modes()[selected.name.as_str()],
            TrackingMode::Untracked
        );
        assert_eq!(app.server_logs.snapshots[&selected.key()].text, "cached\n");
        let mut restored = LogViewer {
            cache_root: app.server_logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_subscriptions();
        assert_eq!(
            restored.mode_for_name(&selected.name),
            TrackingMode::Untracked
        );
        send(&mut app, 't');
        assert_eq!(
            app.server_logs.tracking_mode(&selected),
            TrackingMode::OnDemand
        );
        app.server_logs.file_filter = "no matching file".into();
        send(&mut app, 't');
        assert_eq!(
            app.server_logs.tracking_mode(&selected),
            TrackingMode::OnDemand
        );
        assert!(app.server_logs.message.contains("No matching file"));
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn hosts_pane_participates_in_keyboard_focus_and_filters_content() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{Terminal, backend::TestBackend, style::Color};

        let mut logs = viewer();
        for host in ["one", "two"] {
            let content = format!("2026-10-14 08:00:00.000 Info: {host}-entry\n");
            let mut file = named_file(host, "8000_ErrorLog.txt", "2026-10-14T09:00:00Z");
            file.size = content.len() as u64;
            logs.files.push(file.clone());
            logs.commit(file, true, &content, |_| {}).unwrap();
        }
        logs.select(0);
        logs.toggle_combined();
        logs.files_focus = true;
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        let send = |app: &mut crate::App, code, modifiers| {
            crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
        };
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(app.server_logs.hosts_focus);
        terminal
            .draw(|f| crate::logs_ui::render(f, &mut app.server_logs, true))
            .unwrap();
        let host_area = app
            .server_logs
            .mouse_regions
            .iter()
            .find_map(|(area, target)| matches!(target, LogMouseTarget::Hosts).then_some(*area))
            .unwrap();
        let files_area = app
            .server_logs
            .mouse_regions
            .iter()
            .find_map(|(area, target)| {
                matches!(target, LogMouseTarget::Files { .. }).then_some(*area)
            })
            .unwrap();
        assert!(host_area.bottom() <= files_area.y);
        assert_eq!(
            terminal.backend().buffer()[(host_area.x, host_area.y)].fg,
            Color::Yellow
        );
        send(&mut app, KeyCode::Down, KeyModifiers::NONE);
        send(&mut app, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(!app.server_logs.host_enabled("two"));
        assert!(app.server_logs.visible_text().contains("one-entry"));
        assert!(!app.server_logs.visible_text().contains("two-entry"));
        send(&mut app, KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(app.server_logs.visible_text().contains("two-entry"));
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.server_logs.files_focus && !app.server_logs.hosts_focus);
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!app.server_logs.files_focus && !app.server_logs.hosts_focus);
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.server_logs.hosts_focus);
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(!app.server_logs.files_focus && !app.server_logs.hosts_focus);
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(app.server_logs.files_focus && !app.server_logs.hosts_focus);
        // Split log panes remain in the same cycle, between Files and Hosts.
        app.server_logs.toggle_combined();
        app.server_logs.files_focus = true;
        assert_eq!(app.server_logs.host_views.len(), 2);
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(!app.server_logs.files_focus);
        assert_eq!(app.server_logs.leading_host_index(), Some(0));
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.server_logs.leading_host_index(), Some(1));
        send(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(app.server_logs.hosts_focus);
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(!app.server_logs.files_focus);
        assert_eq!(app.server_logs.leading_host_index(), Some(1));
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(app.server_logs.leading_host_index(), Some(0));
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert!(app.server_logs.files_focus && !app.server_logs.hosts_focus);
        // Hiding the sidebar must not leave keyboard focus on an invisible pane.
        send(&mut app, KeyCode::BackTab, KeyModifiers::SHIFT);
        send(&mut app, KeyCode::Char('\\'), KeyModifiers::NONE);
        assert!(!app.server_logs.sidebar_visible && !app.server_logs.hosts_focus);
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn host_controls_filter_single_merged_and_all_cached_views_and_reach_nine_hosts() {
        use crossterm::event::{
            KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{Terminal, backend::TestBackend};

        let mut logs = viewer();
        for number in 1..=9 {
            let host = format!("host-{number:02}");
            let content = format!("2026-10-14 08:00:00.000 Info: needle-{number:02}\n");
            let mut file = named_file(&host, "8000_ErrorLog.txt", "2026-10-14T09:00:00Z");
            file.size = content.len() as u64;
            logs.files.push(file.clone());
            logs.commit(file, true, &content, |_| {}).unwrap();
        }
        logs.select(0);
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = logs;
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| crate::logs_ui::render(f, &mut app.server_logs, true))
            .unwrap();
        assert_eq!(app.server_logs.host_detail_capacity, 9);
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("h9=host-09"));
        let send = |app: &mut crate::App, code| {
            crate::events::handle_key_event(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap();
        };
        send(&mut app, KeyCode::Char('w'));
        for _ in 0..8 {
            send(&mut app, KeyCode::Down);
        }
        send(&mut app, KeyCode::Char('i'));
        assert_eq!(app.server_logs.host_views.len(), 1);
        assert_eq!(app.server_logs.host_views[0].host, "host-09");
        assert!(app.server_logs.visible_text().contains("needle-09"));
        assert_eq!(app.server_logs.on_demand_requests(None).len(), 9);
        send(&mut app, KeyCode::Up);
        send(&mut app, KeyCode::Char(' '));
        assert_eq!(app.server_logs.host_views.len(), 2);
        assert!(
            app.server_logs
                .host_views
                .iter()
                .all(|view| view.host == "host-08" || view.host == "host-09")
        );
        send(&mut app, KeyCode::Esc);
        app.server_logs.toggle_combined();
        app.server_logs.search = "needle".into();
        app.server_logs.filter();
        let text = app.server_logs.visible_text();
        assert!(text.contains("needle-08") && text.contains("needle-09"));
        assert!(!text.contains("needle-01"));
        app.server_logs.files_focus = true;
        app.server_logs.select_log_group(true);
        let text = app.server_logs.visible_text();
        assert!(
            text.contains("needle-08") && text.contains("needle-09") && !text.contains("needle-01")
        );
        app.server_logs.toggle_combined();
        app.server_logs.toggle_all_cached();
        let text = app.server_logs.visible_text();
        assert!(
            text.contains("needle-08") && text.contains("needle-09") && !text.contains("needle-01")
        );
        app.server_logs.enable_all_hosts();
        assert!(app.server_logs.visible_text().contains("needle-01"));

        let mut small = Terminal::new(TestBackend::new(120, 18)).unwrap();
        app.server_logs.files_focus = true;
        small
            .draw(|f| crate::logs_ui::render(f, &mut app.server_logs, true))
            .unwrap();
        assert!(app.server_logs.host_detail_capacity < 9);
        send(&mut app, KeyCode::Char('w'));
        for _ in 0..8 {
            send(&mut app, KeyCode::Down);
        }
        assert_eq!(app.server_logs.host_selected, 8);
        assert!(app.server_logs.host_detail_offset > 0);
        small
            .draw(|f| crate::logs_ui::render(f, &mut app.server_logs, true))
            .unwrap();
        let (area, _) = app
            .server_logs
            .mouse_regions
            .iter()
            .find(|(_, target)| matches!(target, LogMouseTarget::HostChoice(8)))
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "regions: {:?}, offset {}, capacity {}",
                    app.server_logs.mouse_regions,
                    app.server_logs.host_detail_offset,
                    app.server_logs.host_detail_capacity
                )
            });
        crate::events::handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: area.x,
                row: area.y,
                modifiers: KeyModifiers::NONE,
            },
        )
        .unwrap();
        assert!(!app.server_logs.host_enabled("host-09"));
        assert!(!app.server_logs.visible_text().contains("needle-09"));
        for index in 0..9 {
            let host = app.server_logs.host_names()[index].clone();
            if app.server_logs.host_enabled(&host) {
                app.server_logs.toggle_host(index);
            }
        }
        assert!(app.server_logs.matches.is_empty());
        small
            .draw(|f| crate::logs_ui::render(f, &mut app.server_logs, true))
            .unwrap();
        let screen: String = small
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("No hosts selected"));
        app.server_logs.toggle_host(8);
        assert!(app.server_logs.visible_text().contains("needle-09"));
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
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
    fn appends_finish_the_pinned_local_read_before_loading_newer_bytes() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let raw = format!("2026-10-14 08:00:00.000 Info: seed {}\n", "x".repeat(140)).repeat(7_000);
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        let key = source.key();
        let lock = logs.parse_lock.clone();
        let held = lock.lock().unwrap();
        logs.select(0);
        let first = logs.parse_jobs[&key].progress.clone();
        let cancelled = logs.parse_jobs[&key].cancelled.clone();
        assert!(
            logs.parsing_status()
                .unwrap()
                .contains("Waiting for local log reader")
        );
        for index in 0..4 {
            let appended = format!("2026-10-14 08:01:00.000 Info: live-{index}\n");
            let old = logs
                .commit(source.clone(), false, &appended, |_| {})
                .unwrap();
            logs.extend_view(&key, false, old);
            assert!(
                Arc::ptr_eq(&first, &logs.parse_jobs[&key].progress),
                "an append must not restart the reader"
            );
            assert!(!cancelled.load(Ordering::Relaxed));
        }
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !logs.parsed_large.contains_key(&key) {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(logs.parsed_large[&key].0.bytes, raw.len() as u64);
        assert!(logs.visible_text().contains("seed"));
        assert!(!logs.visible_text().contains("live-3"));
        assert!(
            logs.parse_jobs.contains_key(&key),
            "newer bytes load after the first view is available"
        );
        while !logs.parse_jobs.is_empty() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(logs.visible_text().contains("live-3"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn failed_local_reads_do_not_restart_on_every_view_rebuild() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let raw = format!(
            "2026-10-14 08:00:00.000 Info: repaired {}\n",
            "x".repeat(140)
        )
        .repeat(7_000);
        let mut source = file("one");
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        let key = source.key();
        fs::remove_file(logs.snapshots[&key].content_path(&logs.cache_root)).unwrap();
        logs.select(0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !logs.parse_jobs.is_empty() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(logs.read_failures.contains_key(&key));
        for _ in 0..20 {
            logs.rebuild_lines();
            assert!(logs.parse_jobs.is_empty());
        }
        logs.commit(source, true, &raw, |_| {}).unwrap();
        logs.rebuild_lines();
        assert!(
            !logs.parse_jobs.is_empty(),
            "a replacement generation bypasses the failed-read cooldown"
        );
        while !logs.parse_jobs.is_empty() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!logs.read_failures.contains_key(&key));
        assert!(logs.visible_text().contains("repaired"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn archiving_keeps_an_existing_read_and_its_prepared_view() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let raw = format!(
            "2026-10-13 08:00:00.000 Info: archive-read {}\n",
            "x".repeat(140)
        )
        .repeat(7_000);
        let mut source = named_file("one", "ErrorLog_1.txt", "2026-10-13T12:00:00Z");
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.discovery_days_ago = 1;
        logs.commit(source.clone(), true, &raw, |meta| {
            meta.full_size = Some(source.size);
            meta.end = source.size;
            meta.length = source.size;
        })
        .unwrap();
        let key = source.key();
        let snapshot = logs.snapshots[&key].meta();
        let plain = snapshot.content_path(&logs.cache_root);
        let metadata = fs::read(logs.cache_root.join(format!("{}.json", hash_key(&key)))).unwrap();
        let archive = logs.cache_root.join("archives").join("during-read.gz");
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        archive::build(&plain, &archive, source.size, &AtomicBool::new(false)).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        logs.archive_job = Some(ArchiveJob {
            key: key.clone(),
            name: source.name.clone(),
            cancelled: cancelled.clone(),
        });
        let lock = logs.parse_lock.clone();
        let held = lock.lock().unwrap();
        logs.select(0);
        let reading = logs.parse_jobs[&key].progress.clone();
        logs.archive_tx
            .send(ArchiveResult {
                key: key.clone(),
                generation: snapshot.generation,
                bytes: source.size,
                name: "during-read.gz".into(),
                cancelled,
                metadata,
                result: Ok(ArchiveArtifact {
                    path: archive,
                    keep: false,
                }),
            })
            .unwrap();
        logs.tick_archive(&runtime);
        assert!(
            plain.exists(),
            "keep the original until its queued reader finishes"
        );
        assert!(Arc::ptr_eq(&reading, &logs.parse_jobs[&key].progress));
        drop(held);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !logs.parse_jobs.is_empty() {
            assert!(Instant::now() < deadline);
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(1));
        }
        logs.tick_archive(&runtime);
        assert!(!plain.exists());
        assert!(logs.snapshots[&key].index_path(&logs.cache_root).exists());
        let records = logs.parsed_streams[&key].records.as_ptr();
        logs.rebuild_lines();
        assert!(
            logs.parse_jobs.is_empty(),
            "compression must not invalidate a prepared view"
        );
        assert_eq!(logs.parsed_streams[&key].records.as_ptr(), records);
        assert!(logs.visible_text().contains("archive-read"));
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
            .position(|line| line.contains("----") && line.contains("⬇"))
            .unwrap();
        assert_eq!(row, group + 1);
        assert!(lines[group + 2].contains("h2") && lines[group + 2].contains("2.0 KiB  n/a"));
        assert!(lines[group + 3].contains("8000"));
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
            .position(|line| line.contains("8000") && line.contains("⬇"))
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
    fn calendar_day_selection_excludes_old_entries_and_rebuilds_when_moving_days() {
        let mut logs = viewer_on("2026-10-31");
        for (name, text) in [
            (
                "ErrorLog.txt",
                "2026-10-25 15:38:30.000 Info: stale-current\n2026-10-31 00:00:00.000 Info: today-start\n2026-10-31 23:59:59.999 Info: today-end\n2026-11-01 00:00:00.000 Info: tomorrow\n",
            ),
            (
                "ErrorLog_1.txt",
                "2026-10-30 08:00:00.000 Info: yesterday\n",
            ),
            (
                "ErrorLog_2.txt",
                "2026-10-29 08:00:00.000 Info: two-days-ago\n",
            ),
            (
                "8000_ErrorLog.txt",
                "2026-10-28 08:00:00.000 Info: old-other-port\n2026-10-31 08:00:00.000 Info: today-other-port\n",
            ),
        ] {
            let file = named_file("one", name, "2026-10-31T12:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select_grid_cell("----", 0, 0);
        for merged in [false, true] {
            if merged {
                logs.toggle_combined();
            }
            let text = logs.visible_text();
            assert!(
                text.contains("today-start") && text.contains("today-end"),
                "{text}"
            );
            for excluded in [
                "stale-current",
                "yesterday",
                "two-days-ago",
                "tomorrow",
                "today-other-port",
            ] {
                assert!(!text.contains(excluded), "unexpected {excluded}: {text}");
            }
        }
        logs.select_grid_cell("----", 1, 0);
        assert!(logs.combined);
        assert!(logs.visible_text().contains("yesterday"));
        assert!(!logs.visible_text().contains("today-start"));
        logs.select_grid_cell("----", 0, 0);
        assert!(logs.visible_text().contains("today-start"));
        assert!(!logs.visible_text().contains("yesterday"));
        // A column stays on the selected day across ports.
        logs.select_log_group(false);
        let text = logs.visible_text();
        assert!(text.contains("today-start") && text.contains("today-other-port"));
        assert!(!text.contains("stale-current") && !text.contains("old-other-port"));
        // A row explicitly opens the selected stream across days.
        logs.select_grid_cell("----", 0, 0);
        logs.select_log_group(true);
        let text = logs.visible_text();
        assert!(
            text.contains("stale-current")
                && text.contains("yesterday")
                && text.contains("two-days-ago")
        );
        logs.select_grid_cell("----", 0, 0);
        assert!(!logs.has_log_selection());
        assert!(!logs.visible_text().contains("stale-current"));
        let exported = logs.editor_export().unwrap();
        let text = fs::read_to_string(&exported).unwrap();
        assert!(text.contains("today-start") && !text.contains("stale-current"));
        fs::remove_file(exported).unwrap();
        logs.search = "stale-current".into();
        logs.filter();
        assert!(logs.visible_text().is_empty());
        logs.search.clear();
        let source = logs.files[0].clone();
        logs.commit(
            source,
            true,
            "2026-10-25 15:38:30.000 Info: stale-only\n",
            |_| {},
        )
        .unwrap();
        logs.rebuild_lines();
        assert!(
            logs.visible_text().is_empty(),
            "old content must not fill an empty day"
        );
        logs.toggle_combined();
        assert!(logs.visible_text().is_empty());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("MERGED port ---- · today 2026-10-31"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn calendar_day_selection_uses_server_time_for_access_logs() {
        let mut logs = viewer_on("2026-10-31");
        logs.server_offset = FixedOffset::east_opt(2 * 3600).unwrap();
        let source = named_file("one", "8000_AccessLog.txt", "2026-10-31T12:00:00Z");
        logs.files.push(source.clone());
        logs.commit(
            source,
            true,
            concat!(
                "127.0.0.1 - u [30/Oct/2026:21:59:59 +0000] \"GET /yesterday HTTP/1.1\" 200 1\n",
                "127.0.0.1 - u [30/Oct/2026:22:00:00 +0000] \"GET /today HTTP/1.1\" 200 1\n",
                "127.0.0.1 - u [31/Oct/2026:22:00:00 +0000] \"GET /tomorrow HTTP/1.1\" 200 1\n",
            ),
            |_| {},
        )
        .unwrap();
        logs.select_grid_cell("8000", 0, 1);
        for merged in [false, true] {
            if merged {
                logs.toggle_combined();
            }
            let text = logs.visible_text();
            assert!(text.contains("/today"));
            assert!(!text.contains("/yesterday") && !text.contains("/tomorrow"));
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn calendar_day_selection_limits_background_reads_and_keeps_truncation() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer_on("2026-10-31");
        let text = format!(
            "{}2026-10-31 08:00:00.000 Info: today-before-bound\n2026-10-31 10:00:00.000 Info: today-match\n{}",
            "2026-10-25 08:00:00.000 Info: old-record\n".repeat(30_000),
            "2026-11-01 08:00:00.000 Info: future-record\n".repeat(30_000),
        );
        let mut source = file("one");
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &text, |_| {}).unwrap();
        assert!(logs.snapshots[&source.key()].text.is_empty());
        let window = TimeWindow {
            start: presentation::parse_time("2026-10-31 09:00:00", logs.server_offset),
            end: None,
        };
        logs.apply_window(window, None);
        logs.select_grid_cell("----", 0, 0);
        for merged in [false, true] {
            if merged {
                logs.toggle_combined();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !logs.parse_jobs.is_empty() {
                assert!(
                    Instant::now() < deadline,
                    "background day read did not finish"
                );
                logs.tick(&runtime, false);
                std::thread::sleep(Duration::from_millis(1));
            }
            let text = logs.visible_text();
            assert!(text.contains("today-match"), "merged={merged}: {text}");
            for excluded in ["old-record", "future-record", "today-before-bound"] {
                assert!(!text.contains(excluded), "unexpected {excluded}");
            }
            assert_eq!(logs.window, window);
            assert!(!logs.large_view_limited());
        }
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn calendar_day_selection_updates_after_server_midnight() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer_on("2026-10-31");
        let source = file("one");
        logs.files.push(source.clone());
        logs.commit(source, true, "2026-10-31 23:59:59.999 Info: before-midnight\n2026-11-01 00:00:00.000 Info: after-midnight\n", |_| {}).unwrap();
        for merged in [false, true] {
            logs.clock = Some((
                NaiveDate::from_ymd_opt(2026, 10, 31)
                    .unwrap()
                    .and_hms_opt(23, 59, 59)
                    .unwrap(),
                Instant::now(),
            ));
            logs.select_grid_cell("----", 0, 0);
            if merged {
                logs.toggle_combined();
            }
            assert!(logs.visible_text().contains("before-midnight"));
            assert!(!logs.visible_text().contains("after-midnight"));
            logs.clock = Some((
                NaiveDate::from_ymd_opt(2026, 11, 1)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
                Instant::now(),
            ));
            logs.tick(&runtime, false);
            assert!(logs.visible_text().contains("after-midnight"));
            assert!(!logs.visible_text().contains("before-midnight"));
        }
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn row_selection_searches_one_log_type_across_days_and_hosts() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for (ch, modifiers) in [
            ('V', KeyModifiers::NONE),
            ('V', KeyModifiers::SHIFT),
            ('v', KeyModifiers::SHIFT),
        ] {
            let mut logs = viewer();
            for (host, name, text) in [
                (
                    "one",
                    "8000_ErrorLog.txt",
                    "2026-10-14 08:02:00.000 Info: needle-today\n",
                ),
                (
                    "two",
                    "8000_ErrorLog.txt",
                    "2026-10-14 08:03:00.000 Error: needle-other-host\n2026-10-14 08:03:00.000 Error+: stack-detail\n",
                ),
                (
                    "one",
                    "8000_ErrorLog_1.txt",
                    "2026-10-13 08:00:00.000 Info: before-window\n2026-10-13 08:02:00.000 Info: needle-yesterday\n",
                ),
                (
                    "one",
                    "8000_ErrorLog_9.txt",
                    "2026-10-05 08:02:00.000 Info: older-rotation\n",
                ),
                (
                    "one",
                    "9000_ErrorLog.txt",
                    "2026-10-14 08:04:00.000 Info: other-port\n",
                ),
                (
                    "one",
                    "8000_RequestLog.txt",
                    "{\"time\":\"2026-10-14T08:04:00Z\",\"message\":\"other-type\"}\n",
                ),
            ] {
                let file = named_file(host, name, "2026-10-14T09:00:00Z");
                logs.files.push(file.clone());
                logs.commit(file, true, text, |_| {}).unwrap();
            }
            logs.select(0);
            let window = TimeWindow {
                start: presentation::parse_time("2026-10-13 08:01:00", logs.server_offset),
                end: None,
            };
            logs.apply_window(window, None);
            let cursor = logs.ensure_cursor_time();
            let mut app = crate::tests::test_app();
            app.mode = crate::AppMode::MarkLogicLogs;
            app.server_logs = logs;
            let send = |app: &mut crate::App, code, modifiers| {
                crate::events::handle_key_event(app, KeyEvent::new(code, modifiers)).unwrap();
            };
            send(&mut app, KeyCode::Char(ch), modifiers);
            let logs = &app.server_logs;
            assert!(logs.files_focus && logs.combined && !logs.busy);
            assert_eq!(logs.log_selection_count(), 3); // One filename per day, all hosts.
            assert_eq!(logs.window, window);
            assert_eq!(logs.cursor_time, cursor);
            let text = logs.visible_text();
            for entry in [
                "needle-yesterday",
                "needle-today",
                "needle-other-host",
                "stack-detail",
            ] {
                assert!(text.contains(entry), "missing {entry}: {text}");
            }
            assert!(text.find("needle-yesterday").unwrap() < text.find("needle-today").unwrap());
            for entry in [
                "before-window",
                "older-rotation",
                "other-port",
                "other-type",
            ] {
                assert!(!text.contains(entry), "unexpected {entry}: {text}");
            }
            send(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            send(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
            for ch in "stack-detail".chars() {
                send(&mut app, KeyCode::Char(ch), KeyModifiers::NONE);
            }
            send(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            let text = app.server_logs.visible_text();
            assert!(text.contains("needle-other-host") && text.contains("stack-detail"));
            assert!(!text.contains("needle-today") && !text.contains("needle-yesterday"));
            let snapshots = app.server_logs.snapshots.len();
            send(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            assert!(!app.server_logs.has_log_selection() && !app.server_logs.combined);
            assert_eq!(app.server_logs.snapshots.len(), snapshots);
            fs::remove_dir_all(app.server_logs.cache_root).unwrap();
        }
    }

    #[test]
    fn column_selection_searches_request_logs_on_one_day_and_respects_filters() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        for (ch, modifiers) in [
            ('C', KeyModifiers::NONE),
            ('C', KeyModifiers::SHIFT),
            ('c', KeyModifiers::SHIFT),
        ] {
            let mut logs = viewer();
            for (host, name, value) in [
                ("one", "8000_RequestLog_1.txt", "selected-one"),
                ("two", "8000_RequestLog_1.txt", "selected-two"),
                ("one", "9000_RequestLog_1.txt", "selected-other-port"),
                ("one", "TaskServer_RequestLog_1.txt", "selected-task"),
                ("one", "9001_RequestLog_1.txt", "excluded-port"),
                ("one", "9000_RequestLog.txt", "other-day"),
                ("one", "8000_ErrorLog_1.txt", "other-type"),
                ("one", "Custom_RequestLog_1.txt", "not-in-grid"),
            ] {
                let file = named_file(host, name, "2026-10-14T09:00:00Z");
                logs.files.push(file.clone());
                logs.commit(
                    file,
                    true,
                    &format!("{{\"time\":\"2026-10-13T08:02:00Z\",\"request\":\"{value}\"}}\n"),
                    |_| {},
                )
                .unwrap();
            }
            logs.port_filters.insert("9001".into(), PortFilter::Exclude);
            logs.select_grid_cell("8000", 1, 2);
            let snapshots = logs.snapshots.len();
            let mut app = crate::tests::test_app();
            app.mode = crate::AppMode::MarkLogicLogs;
            app.server_logs = logs;
            crate::events::handle_key_event(&mut app, KeyEvent::new(KeyCode::Char(ch), modifiers))
                .unwrap();
            let logs = &mut app.server_logs;
            assert!(logs.has_log_selection() && logs.files_focus && logs.combined);
            assert_eq!(logs.log_selection_count(), 3);
            assert_eq!(logs.selected_kind_column(), 2);
            assert_eq!(logs.selected_display_column(), 2);
            assert_eq!(logs.snapshots.len(), snapshots); // C must never clear the cache in Files.
            assert_eq!(logs.discovery_days_ago, 1);
            logs.search = "selected".into();
            logs.filter();
            let text = logs.visible_text();
            for entry in [
                "selected-one",
                "selected-two",
                "selected-other-port",
                "selected-task",
            ] {
                assert!(text.contains(entry), "missing {entry}: {text}");
            }
            for entry in ["excluded-port", "other-day", "other-type", "not-in-grid"] {
                assert!(!text.contains(entry), "unexpected {entry}: {text}");
            }
            logs.file_filter = "9000".into();
            logs.select_grid_cell("9000", 1, 2);
            assert!(!logs.has_log_selection() && !logs.combined);
            logs.select_log_group(false);
            assert_eq!(logs.log_selection_count(), 1);
            assert!(logs.selected_log_names.contains("9000_RequestLog_1.txt"));
            fs::remove_dir_all(&logs.cache_root).unwrap();
        }
    }

    #[test]
    fn log_selection_survives_sorting_and_updates_but_clears_on_file_navigation() {
        let mut logs = viewer();
        for name in [
            "8000_ErrorLog.txt",
            "8000_ErrorLog_1.txt",
            "9000_ErrorLog.txt",
        ] {
            let file = named_file("one", name, "2026-10-14T09:00:00Z");
            logs.files.push(file.clone());
            logs.commit(
                file,
                true,
                "2026-10-14 08:00:00.000 Info: initial\n",
                |_| {},
            )
            .unwrap();
        }
        logs.select(0);
        logs.select_log_group(true);
        let names = logs.selected_log_names.clone();
        logs.files.reverse();
        logs.sort_files();
        logs.rebuild_lines();
        assert_eq!(logs.selected_log_names, names);
        let source = logs
            .files
            .iter()
            .find(|file| file.name == "8000_ErrorLog_1.txt")
            .unwrap()
            .clone();
        logs.commit(
            source.clone(),
            false,
            "2026-10-14 08:01:00.000 Info: appended\n",
            |_| {},
        )
        .unwrap();
        logs.extend_view(&source.key(), false, 0);
        logs.rebuild_lines();
        assert!(logs.visible_text().contains("appended"));
        logs.select_grid_cell("8000", 1, 0);
        assert!(!logs.combined && !logs.has_log_selection());
        assert_eq!(logs.selected_file().unwrap().name, "8000_ErrorLog_1.txt");
        logs.select_log_group(true);
        logs.toggle_combined();
        assert!(!logs.combined && !logs.has_log_selection());
        logs.file_filter = "no matches".into();
        logs.select_log_group(false);
        assert!(!logs.has_log_selection());
        assert!(logs.message.contains("No matching file"));
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
        for entry in ["access-one", "access-two", "error-two", "stack-detail"] {
            assert!(text.contains(entry), "missing {entry}: {text}");
        }
        for entry in [
            "error-one",
            "rotation-inside",
            "before-window",
            "after-window",
            "request-excluded",
            "other-port-error",
            "other-port-access",
        ] {
            assert!(!text.contains(entry), "unexpected {entry}: {text}");
        }
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
    fn selecting_a_rotation_keeps_the_merged_view_and_does_not_start_discovery() {
        let mut logs = viewer_on("2026-10-27");
        for (name, text) in [
            (
                "9061_ErrorLog.txt",
                "2026-10-27 01:30:02.454 Info: today-entry\n",
            ),
            (
                "9061_ErrorLog_1.txt",
                "2026-10-26 01:30:02.454 Info: yesterday-entry\n",
            ),
        ] {
            let file = named_file("one", name, "2026-10-27T02:00:00Z");
            logs.files.push(file.clone());
            logs.commit(file, true, text, |_| {}).unwrap();
        }
        logs.select(0);
        logs.toggle_combined();
        let rows = logs.rows.len();
        let revision = logs.discovery_revision;
        logs.select_grid_cell("9061", 1, 0);
        assert_eq!(logs.discovery_days_ago, 1);
        assert_eq!(logs.selected_file().unwrap().name, "9061_ErrorLog_1.txt");
        assert_eq!(logs.discovery_revision, revision);
        assert!(!logs.busy);
        assert_eq!(logs.rows.len(), rows);
        assert!(logs.visible_text().contains("yesterday-entry"));
        assert!(!logs.visible_text().contains("today-entry"));
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
                "9061_ErrorLog.txt",
                "2026-10-26 20:28:10.000 Info: previous-day\n",
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
        logs.select_log_group(true);
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
        let mut logs = viewer_on("2026-10-28");
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
    fn complete_inventory_confirms_deleted_historical_files() {
        let mut logs = viewer();
        let file = named_file("one", "TaskServer_ErrorLog_1.txt", "2026-10-13T09:00:00Z");
        logs.files.push(file.clone());
        logs.commit(file, true, "cached", |_| {}).unwrap();
        let mut inventory = response("one", vec![]);
        inventory.files.clear();
        logs.tx
            .send(CollectorEvent::Inventory {
                host: "one".into(),
                result: Ok(inventory),
                bytes: 0,
                revision: logs.discovery_revision,
                days_ago: u32::MAX,
                show_untracked: true,
            })
            .unwrap();
        let runtime = Runtime::new().unwrap();
        logs.tick(&runtime, true);
        assert!(!logs.files[0].present);
        assert_eq!(
            logs.grid_freshness(Utc::now())["TaskServer_ErrorLog_1.txt"],
            CacheFreshness::Fresh
        );
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
        for directory in ["archives", ".staging"] {
            fs::create_dir(logs.cache_root.join(directory)).unwrap();
            fs::write(logs.cache_root.join(directory).join("content"), "cached").unwrap();
        }

        let runtime = Runtime::new().unwrap();
        logs.clear_cached_logs(&runtime);

        assert!(logs.snapshots.is_empty());
        assert!(logs.lines.is_empty());
        assert_eq!(logs.scope_days, 7);
        assert!(logs.cache_root.join(SUBSCRIPTIONS_FILE).exists());
        assert!(!logs.cache_root.join("saved-view.txt").exists());
        assert!(!logs.cache_root.join("archives").exists());
        assert!(!logs.cache_root.join(".staging").exists());
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
        send(&mut app, KeyCode::Char('D'), KeyModifiers::SHIFT);
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
        let bar: String = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 21)].symbol())
            .collect();
        assert!(bar.contains("~50%"), "{bar}");
        assert_eq!(terminal.backend().buffer()[(0, 21)].symbol(), "█");
        assert_eq!(terminal.backend().buffer()[(119, 21)].symbol(), " ");

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
    fn selected_download_is_queued_while_busy() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        app.server_logs = viewer();
        let selected = named_file("one", "8000_ErrorLog.txt", "2026-10-14T12:00:00Z");
        app.server_logs.files.push(selected.clone());
        app.server_logs
            .modes
            .insert(selected.name.clone(), TrackingMode::Live);
        app.server_logs.files_focus = true;
        app.server_logs.combined = true;
        app.server_logs.select(0);
        app.server_logs.busy = true;
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(
            app.server_logs
                .manual_downloads_queued
                .contains(&selected.name)
        );
        assert!(app.server_logs.message.contains("queued"));
        assert!(app.server_logs.merged_downloads_queued.is_empty());
        app.server_logs.cancel();
        assert!(app.server_logs.manual_downloads_queued.is_empty());
        fs::remove_dir_all(&app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn manual_download_interrupts_startup_cache_preservation_and_reaches_the_server() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        let listener = app
            .rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = app.rt.spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") { headers.push(stream.read_u8().await.unwrap()); }
            let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).await.unwrap();
            let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
            let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
            let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0]["path"], "/Logs/8000_ErrorLog_3.txt");
            assert_eq!(requests[0]["full"], true);
            let response = json!({"host":"one","directory":"/Logs","inventory":false,"read_bytes":3,"cluster_bytes":0,
                "files":[{"host":"one","path":"/Logs/8000_ErrorLog_3.txt","name":"8000_ErrorLog_3.txt","size":3,"modified":"2026-10-11T09:00:00Z"}],
                "chunks":[{"host":"one","path":"/Logs/8000_ErrorLog_3.txt","start":0,"end":3,"length":3,"reset":true,"text":"abc","full":true}]}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        let file = named_file("one", "8000_ErrorLog_3.txt", "2026-10-11T09:00:00Z");
        logs.files.push(file.clone());
        logs.modes
            .insert("8000_ErrorLog.txt".into(), TrackingMode::Untracked);
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.select_grid_cell("8000", 3, 0);
        logs.files_focus = true;
        logs.hosts_focus = true;
        let cancelled = Arc::new(AtomicBool::new(false));
        logs.history_cancel = Some(cancelled.clone());
        logs.history_running = Some(logs.now().date());
        logs.begin_activity("Preserving rotated caches", 4);
        let old_sender = logs.tx.clone();
        app.server_logs = logs;
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(!app.server_logs.manual_downloads_queued.contains(&file.name));
        assert!(cancelled.load(Ordering::Relaxed));
        assert_eq!(
            app.server_logs.activity.as_ref().unwrap().label,
            "Downloading logs"
        );
        assert!(
            old_sender
                .send(CollectorEvent::Done { auto_tail: false })
                .is_err()
        );
        assert_eq!(
            app.server_logs.tracking_mode(&file),
            TrackingMode::Untracked
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 24)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let gauge: String = (0..160)
            .map(|x| terminal.backend().buffer()[(x, 21)].symbol())
            .collect();
        // The local server can finish transferring before this frame is drawn;
        // completion still waits for the collector's Done event to be applied.
        assert!(gauge.contains("0/1 done"), "{gauge}");
        assert!(gauge.contains("Q: queue"), "{gauge}");
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while app.server_logs.busy {
                    app.server_logs.tick(&app.rt, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                server.await.unwrap();
            })
            .await
            .unwrap();
        });
        assert_eq!(app.server_logs.snapshots[&file.key()].text, "abc");
        assert_eq!(
            app.server_logs.tracking_mode(&file),
            TrackingMode::Untracked
        );
        assert!(
            app.server_logs.message.contains("successful"),
            "{}",
            app.server_logs.message
        );
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn downloads_dialog_can_start_a_waiting_file_and_resume_interrupted_downloads() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        let listener = app
            .rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = app.rt.spawn(async move {
            let mut started_tx = Some(started_tx);
            let mut stalled = None;
            let mut paths = Vec::new();
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") { headers.push(stream.read_u8().await.unwrap()); }
                let length: usize = String::from_utf8_lossy(&headers).lines().find_map(|line| line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)).unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                let vars: Value = serde_json::from_str(&form["vars"]).unwrap();
                let requests: Vec<Value> = serde_json::from_str(vars["requests"].as_str().unwrap()).unwrap();
                assert_eq!(requests.len(), 1);
                assert_eq!(requests[0]["full"], true);
                let path = requests[0]["path"].as_str().unwrap();
                let name = path.rsplit('/').next().unwrap();
                paths.push(path.to_owned());
                if index == 0 {
                    started_tx.take().unwrap().send(()).unwrap();
                    stalled = Some(tokio::spawn(async move {
                        // This server deliberately never responds. Starting another
                        // queue entry must cancel it rather than wait for a timeout.
                        assert_eq!(stream.read(&mut [0u8; 1]).await.unwrap(), 0);
                    }));
                    continue;
                }
                let response = json!({"host":"one","directory":"/Logs","inventory":false,"read_bytes":3,"cluster_bytes":0,
                    "files":[{"host":"one","path":path,"name":name,"size":3,"modified":"2026-10-14T09:00:00Z"}],
                    "chunks":[{"host":"one","path":path,"start":0,"end":3,"length":3,"reset":true,"text":"abc","full":true}]}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
            stalled.unwrap().await.unwrap();
            paths
        });
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        let mut logs = viewer();
        logs.files = vec![
            named_file("one", "8000_ErrorLog.txt", "2026-10-14T09:00:00Z"),
            named_file("one", "9000_ErrorLog.txt", "2026-10-14T09:00:00Z"),
        ];
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.files_focus = true;
        logs.select(0);
        app.server_logs = logs;
        let send = |app: &mut crate::App, code| {
            crate::events::handle_key_event(app, KeyEvent::new(code, KeyModifiers::NONE)).unwrap()
        };
        send(&mut app, KeyCode::Char('d'));
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), started_rx)
                .await
                .unwrap()
                .unwrap();
        });
        app.server_logs.tick(&app.rt, true);
        app.server_logs.select(1);
        send(&mut app, KeyCode::Char('d'));
        assert!(
            app.server_logs
                .manual_downloads_queued
                .contains("9000_ErrorLog.txt")
        );
        send(&mut app, KeyCode::Char('Q'));
        assert!(app.server_logs.download_queue_open);
        let rows = app.server_logs.download_queue_rows();
        assert!(
            rows.iter()
                .any(|row| row.contains("WAITING FOR SERVER") && row.contains("8000_ErrorLog.txt"))
        );
        let queued = rows
            .iter()
            .position(|row| row.starts_with("QUEUED 9000_ErrorLog.txt"))
            .unwrap();
        for _ in 0..queued {
            send(&mut app, KeyCode::Down);
        }
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(140, 28)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut app.server_logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(screen.contains("Downloads · Enter/r: start now"));
        assert!(screen.contains("QUEUED 9000_ErrorLog.txt"));
        send(&mut app, KeyCode::Enter);
        assert!(
            app.server_logs
                .manual_downloads_queued
                .contains("8000_ErrorLog.txt")
        );
        assert!(
            app.server_logs
                .activity
                .as_ref()
                .unwrap()
                .jobs
                .contains_key(&("one".into(), "9000_ErrorLog.txt".into()))
        );
        app.rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), async {
                while app.server_logs.busy || !app.server_logs.manual_downloads_queued.is_empty() {
                    app.server_logs.tick(&app.rt, true);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                assert_eq!(
                    server.await.unwrap(),
                    [
                        "/Logs/8000_ErrorLog.txt",
                        "/Logs/9000_ErrorLog.txt",
                        "/Logs/8000_ErrorLog.txt"
                    ]
                );
            })
            .await
            .unwrap();
        });
        for file in &app.server_logs.files {
            assert_eq!(app.server_logs.snapshots[&file.key()].text, "abc");
        }
        send(&mut app, KeyCode::Esc);
        assert!(!app.server_logs.download_queue_open);
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
    }

    #[test]
    fn queued_downloads_run_before_startup_history_and_can_be_removed_in_the_dialog() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = crate::tests::test_app();
        app.mode = crate::AppMode::MarkLogicLogs;
        let mut logs = viewer();
        let client = test_client();
        logs.client = Some(client.clone());
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.inventoried_hosts.insert("one".into());
        logs.files = vec![
            file("one"),
            named_file("one", "ErrorLog_3.txt", "2026-10-11T09:00:00Z"),
        ];
        logs.manual_downloads_queued.insert("ErrorLog.txt".into());
        logs.begin_activity("Discovering logs", 1);
        logs.tx
            .send(CollectorEvent::Done { auto_tail: false })
            .unwrap();
        logs.tick(&app.rt, true);
        assert_eq!(logs.activity.as_ref().unwrap().label, "Downloading logs");
        assert!(logs.history_running.is_none());
        logs.manual_downloads_queued.insert("ErrorLog_3.txt".into());
        app.server_logs = logs;
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE),
        )
        .unwrap();
        app.server_logs.download_queue_selected = app
            .server_logs
            .download_queue_rows()
            .iter()
            .position(|row| row.starts_with("QUEUED ErrorLog_3.txt"))
            .unwrap();
        crate::events::handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        )
        .unwrap();
        assert!(
            !app.server_logs
                .manual_downloads_queued
                .contains("ErrorLog_3.txt")
        );
        assert!(app.server_logs.busy);
        app.server_logs.cancel();
        fs::remove_dir_all(app.server_logs.cache_root).unwrap();
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
    #[test]
    fn disk_index_matches_stream_and_reuses_committed_generation() {
        let logs = viewer();
        let file = file("one");
        let raw = "2026-10-14 08:03:00.000 Info: third\r\n stack trace\r\n2026-10-14 08:01:00.000 Error: first 🦀\n2026-10-14 08:02:00.000 Error+: continuation\n2026-10-14 08:05:00.000 Info: fifth\n";
        let path = logs.cache_root.join("source.log");
        let index_path = logs.cache_root.join("source.sqlite");
        fs::write(&path, raw).unwrap();
        let offset = FixedOffset::east_opt(0).unwrap();
        let start = presentation::parse_time("2026-10-14 08:01:00", offset);
        let end = presentation::parse_time("2026-10-14 08:03:00", offset);
        let spec = ParseSpec {
            generation: 1,
            bytes: raw.len() as u64,
            offset,
            window: TimeWindow { start, end },
            before: None,
            paged: false,
            filter_id: 0,
        };
        let progress = AtomicU64::new(0);
        let stop = AtomicBool::new(false);
        let (indexed, limited) =
            index::read(&path, &index_path, &file, spec, &progress, &stop, None).unwrap();
        let (scan, _) = presentation::Stream::read_window(
            std::io::BufReader::new(raw.as_bytes()),
            &file,
            offset,
            start,
            end,
            &AtomicU64::new(0),
            &stop,
        )
        .unwrap();
        assert!(!limited);
        assert_eq!(
            indexed
                .records
                .iter()
                .map(|r| (&r.raw, r.timestamp))
                .collect::<Vec<_>>(),
            scan.records
                .iter()
                .map(|r| (&r.raw, r.timestamp))
                .collect::<Vec<_>>()
        );
        let before = fs::metadata(&index_path).unwrap().modified().unwrap();
        let (second, _) = index::read(
            &path,
            &index_path,
            &file,
            ParseSpec {
                window: TimeWindow {
                    start: None,
                    end: None,
                },
                ..spec
            },
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        assert_eq!(second.records.len(), 3);
        assert_eq!(
            fs::metadata(&index_path).unwrap().modified().unwrap(),
            before
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn disk_index_reads_pages_from_seekable_gzip_archive() {
        let logs = viewer();
        let source = file("one");
        let raw = format!("2026-10-14 08:01:00.000 Info: {}\n", "x".repeat(90)).repeat(38_000);
        let path = logs.cache_root.join("source.log");
        let archive = logs.cache_root.join("source.log.gz");
        let index_path = logs.cache_root.join("source.sqlite");
        fs::write(&path, &raw).unwrap();
        let stop = AtomicBool::new(false);
        archive::build(&path, &archive, raw.len() as u64, &stop).unwrap();
        let spec = ParseSpec {
            generation: 1,
            bytes: raw.len() as u64,
            offset: FixedOffset::east_opt(0).unwrap(),
            window: TimeWindow::default(),
            before: None,
            paged: true,
            filter_id: 0,
        };
        let (plain, plain_limited) = index::read(
            &path,
            &index_path,
            &source,
            spec,
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        let (compressed, compressed_limited) = index::read(
            &archive,
            &index_path,
            &source,
            spec,
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        assert!(plain_limited && compressed_limited);
        assert_eq!(plain.index_first, compressed.index_first);
        assert_eq!(plain.index_last, compressed.index_last);
        assert_eq!(
            plain
                .records
                .iter()
                .map(|record| &record.raw)
                .collect::<Vec<_>>(),
            compressed
                .records
                .iter()
                .map(|record| &record.raw)
                .collect::<Vec<_>>()
        );
        let (rebuilt, rebuilt_limited) = index::read(
            &archive,
            &logs.cache_root.join("archive.sqlite"),
            &source,
            spec,
            &AtomicU64::new(0),
            &stop,
            None,
        )
        .unwrap();
        assert!(rebuilt_limited);
        assert_eq!(rebuilt.index_first, plain.index_first);
        assert_eq!(rebuilt.index_last, plain.index_last);
        assert_eq!(rebuilt.records.len(), plain.records.len());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn published_archive_survives_restart_and_serves_view_reports_and_export() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let text = format!("2026-10-14 08:01:00.000 Error: {}\n", "x".repeat(100)).repeat(9_000);
        assert!(text.len() as u64 > BACKGROUND_PARSE_BYTES);
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &text, |meta| {
            meta.full_size = Some(source.size);
        })
        .unwrap();
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let archive_dir = logs.cache_root.join("archives");
        fs::create_dir_all(&archive_dir).unwrap();
        archive::build(
            &raw,
            &archive_dir.join("one.gz"),
            source.size,
            &AtomicBool::new(false),
        )
        .unwrap();
        logs.publish_archive(&source.key(), 0, source.size, "one.gz".into())
            .unwrap();
        assert!(raw.exists(), "publication must preserve the original");
        fs::remove_file(&raw).unwrap();

        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            clock: logs.clock,
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(
            restored.snapshots[&source.key()].archive.as_deref(),
            Some("one.gz")
        );
        assert!(restored.snapshots[&source.key()].text.is_empty());
        let deadline = Instant::now() + Duration::from_secs(20);
        while !restored.parse_jobs.is_empty() && Instant::now() < deadline {
            restored.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(5));
        }
        restored.tick(&runtime, true);
        assert!(restored.parse_jobs.is_empty());
        assert!(restored.visible_text().contains("xxxxxxxxxx"));
        let exported = restored.editor_export().unwrap();
        assert_eq!(
            fs::read_to_string(&exported).unwrap(),
            restored.visible_text()
        );
        fs::remove_file(exported).unwrap();
        let mut lines = 0;
        let mut warnings = Vec::new();
        assert_eq!(
            visit_cached_error_logs(&restored.cache_root, |_, _| lines += 1, &mut warnings)
                .unwrap(),
            1
        );
        assert_eq!(lines, 9_000);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(
            restored
                .commit(source.clone(), false, "new\n", |_| {})
                .is_err()
        );
        assert_eq!(
            restored.snapshots[&source.key()].archive.as_deref(),
            Some("one.gz")
        );
        restored.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn archive_publication_checks_generation_and_recovers_when_sidecar_is_missing() {
        let mut logs = viewer();
        let mut source = file("one");
        source.size = 4;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4)
        })
        .unwrap();
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let archive_dir = logs.cache_root.join("archives");
        fs::create_dir_all(&archive_dir).unwrap();
        archive::build(
            &raw,
            &archive_dir.join("one.gz"),
            4,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            logs.publish_archive(&source.key(), 1, 4, "one.gz".into())
                .is_err()
        );
        assert!(
            logs.publish_archive(&source.key(), 0, 4, "../one.gz".into())
                .is_err()
        );
        logs.publish_archive(&source.key(), 0, 4, "one.gz".into())
            .unwrap();
        fs::remove_file(archive_dir.join("one.gz.index.json")).unwrap();
        let mut visited = Vec::new();
        let mut warnings = Vec::new();
        visit_cached_error_logs(
            &logs.cache_root,
            |_, line| visited.push(line.to_owned()),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(visited, ["old"]);
        assert!(warnings.is_empty());
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        assert_eq!(restored.snapshots[&source.key()].archive, None);
        assert_eq!(restored.snapshots[&source.key()].text, "old\n");
        restored
            .commit(source.clone(), false, "new\n", |_| {})
            .unwrap();
        assert_eq!(fs::read_to_string(raw).unwrap(), "old\nnew\n");
        assert_eq!(restored.snapshots[&source.key()].full_size, None);
        restored.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn corrupt_large_archive_falls_back_to_original_generation() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let text = format!("2026-10-14 08:01:00.000 Info: {}\n", "x".repeat(100)).repeat(9_000);
        source.size = text.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &text, |meta| {
            meta.full_size = Some(source.size)
        })
        .unwrap();
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let archive_dir = logs.cache_root.join("archives");
        fs::create_dir_all(&archive_dir).unwrap();
        let archive = archive_dir.join("one.gz");
        archive::build(&raw, &archive, source.size, &AtomicBool::new(false)).unwrap();
        logs.publish_archive(&source.key(), 0, source.size, "one.gz".into())
            .unwrap();
        let mut data = fs::read(&archive).unwrap();
        let last = data.len() - 1;
        data[last] ^= 1;
        fs::write(archive, data).unwrap();
        let mut restored = LogViewer {
            cache_root: logs.cache_root.clone(),
            clock: logs.clock,
            ..LogViewer::default()
        };
        restored.load_cache().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !restored.parse_jobs.is_empty() && Instant::now() < deadline {
            restored.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(5));
        }
        restored.tick(&runtime, true);
        assert!(restored.parse_jobs.is_empty());
        assert!(restored.visible_text().contains("xxxxxxxxxx"));
        assert!(restored.message.contains("Archive unavailable"));
        restored.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    fn exercise_automatic_archive(remote_changes: bool) {
        let runtime = Runtime::new().unwrap();
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut source = named_file("one", "ErrorLog_3.txt", "2026-10-10T00:00:00Z");
        let text = "2026-10-10 00:00:00.000 Error: older\n".repeat(100);
        source.size = text.len() as u64;
        let served = source.clone();
        let server = runtime.spawn(async move {
            for index in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    header.push(stream.read_u8().await.unwrap());
                }
                let length: usize = String::from_utf8_lossy(&header).lines().find_map(|line| {
                    line.to_lowercase().strip_prefix("content-length: ").map(str::to_owned)
                }).unwrap().parse().unwrap();
                let mut body = vec![0; length];
                stream.read_exact(&mut body).await.unwrap();
                let form: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
                assert_eq!(form["xquery"], INVENTORY);
                let mut file = served.clone();
                if remote_changes && index == 1 {
                    file.modified = "2026-10-11T00:00:00Z".into();
                }
                let response = json!({"host":"one", "directory":"/Logs", "inventory":false,
                    "read_bytes":0, "cluster_bytes":0, "files":[file], "chunks":[]}).to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).as_bytes()).await.unwrap();
            }
        });
        let mut logs = viewer();
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &text, |meta| {
            meta.full_size = Some(source.size);
            meta.end = text.chars().count() as u64;
            meta.length = meta.end;
        })
        .unwrap();
        // This fixture exercises legacy, not-yet-settled generations.
        logs.snapshots.get_mut(&source.key()).unwrap().updated_at = Some(source.modified.clone());
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let mut client = test_client();
        client.server.port = port;
        client.server.auth_type = AuthType::Basic;
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client,
        });
        logs.tick_archive(&runtime);
        assert!(logs.archive_job.is_some());
        let deadline = Instant::now() + Duration::from_secs(20);
        while logs.archive_job.is_some() && Instant::now() < deadline {
            logs.tick_archive(&runtime);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(logs.archive_job.is_none(), "archive worker did not finish");
        runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), server)
                .await
                .unwrap()
                .unwrap()
        });
        let snapshot = &logs.snapshots[&source.key()];
        if remote_changes {
            assert!(snapshot.archive.is_none());
            assert_eq!(fs::read_to_string(&raw).unwrap(), text);
        } else {
            assert!(snapshot.archive.is_some(), "{}", logs.message);
            assert!(!raw.exists(), "{}", logs.message);
            let mut actual = String::new();
            snapshot
                .open_content(&logs.cache_root)
                .unwrap()
                .read_to_string(&mut actual)
                .unwrap();
            assert_eq!(actual, text);
        }
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn unsettled_archive_rechecks_remote_and_reclaims_raw_bytes() {
        exercise_automatic_archive(false);
    }

    #[test]
    fn unsettled_archive_retains_raw_when_remote_changes() {
        exercise_automatic_archive(true);
    }

    #[test]
    fn automatic_archive_rejects_a_replaced_local_generation() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = named_file("one", "ErrorLog_3.txt", "2026-10-10T00:00:00Z");
        source.size = 4;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        let metadata = fs::read(
            logs.cache_root
                .join(format!("{}.json", hash_key(&source.key()))),
        )
        .unwrap();
        let old = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let directory = logs.cache_root.join("archives");
        fs::create_dir_all(&directory).unwrap();
        let archive = directory.join("stale.gz");
        archive::build(&old, &archive, 4, &AtomicBool::new(false)).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        logs.archive_job = Some(ArchiveJob {
            key: source.key(),
            name: source.name.clone(),
            cancelled: cancelled.clone(),
        });
        logs.commit(source.clone(), true, "new\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        assert!(
            logs.archive_tx
                .send(ArchiveResult {
                    key: source.key(),
                    generation: 0,
                    bytes: 4,
                    name: "stale.gz".into(),
                    cancelled,
                    metadata,
                    result: Ok(ArchiveArtifact {
                        path: archive.clone(),
                        keep: false
                    }),
                })
                .is_ok()
        );
        logs.tick_archive(&runtime);
        assert!(!archive.exists());
        assert_eq!(logs.snapshots[&source.key()].generation, 1);
        assert!(logs.snapshots[&source.key()].archive.is_none());
        assert_eq!(
            fs::read_to_string(logs.snapshots[&source.key()].content_path(&logs.cache_root))
                .unwrap(),
            "new\n"
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn automatic_archive_skips_partial_recent_and_current_files() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        for (name, modified, complete) in [
            ("ErrorLog_3.txt", "2026-10-10T00:00:00Z", false),
            ("8000_ErrorLog_1.txt", "2026-10-13T00:00:00Z", true),
            ("ErrorLog.txt", "2026-10-10T00:00:00Z", true),
        ] {
            let mut source = named_file("one", name, modified);
            source.size = 4;
            logs.files.push(source.clone());
            logs.commit(source, true, "old\n", |meta| {
                meta.full_size = complete.then_some(4);
                meta.end = 4;
                meta.length = 4;
            })
            .unwrap();
        }
        for snapshot in logs.snapshots.values_mut() {
            snapshot.updated_at = Some(snapshot.file.modified.clone());
        }
        logs.endpoints.push(Endpoint {
            host: "one".into(),
            client: test_client(),
        });
        logs.tick_archive(&runtime);
        assert!(logs.archive_job.is_none());
        assert!(
            logs.snapshots
                .values()
                .all(|snapshot| snapshot.archive.is_none())
        );
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn automatic_archive_waits_for_another_storage_operation() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = named_file("one", "ErrorLog_3.txt", "2026-10-10T00:00:00Z");
        source.size = 4;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, "old\n", |meta| {
            meta.full_size = Some(4);
            meta.end = 4;
            meta.length = 4;
        })
        .unwrap();
        let metadata = fs::read(
            logs.cache_root
                .join(format!("{}.json", hash_key(&source.key()))),
        )
        .unwrap();
        let raw = logs.snapshots[&source.key()].content_path(&logs.cache_root);
        let directory = logs.cache_root.join("archives");
        fs::create_dir_all(&directory).unwrap();
        let archive = directory.join("blocked.gz");
        archive::build(&raw, &archive, 4, &AtomicBool::new(false)).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        logs.archive_job = Some(ArchiveJob {
            key: source.key(),
            name: source.name.clone(),
            cancelled: cancelled.clone(),
        });
        assert!(
            logs.archive_tx
                .send(ArchiveResult {
                    key: source.key(),
                    generation: 0,
                    bytes: 4,
                    name: "blocked.gz".into(),
                    cancelled,
                    metadata,
                    result: Ok(ArchiveArtifact {
                        path: archive.clone(),
                        keep: false
                    }),
                })
                .is_ok()
        );
        let lock = ArchivePublishLock::acquire(&logs.cache_root).unwrap();
        logs.tick_archive(&runtime);
        assert!(!archive.exists());
        assert!(logs.snapshots[&source.key()].archive.is_none());
        assert_eq!(fs::read_to_string(&raw).unwrap(), "old\n");
        drop(lock);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }

    #[test]
    fn cancelled_index_rebuild_keeps_previous_index() {
        let logs = viewer();
        let file = file("one");
        let path = logs.cache_root.join("source.log");
        let index_path = logs.cache_root.join("source.sqlite");
        let raw = "2026-10-14 08:01:00.000 Info: first\n";
        fs::write(&path, raw).unwrap();
        let spec = ParseSpec {
            generation: 0,
            bytes: raw.len() as u64,
            offset: FixedOffset::east_opt(0).unwrap(),
            window: TimeWindow {
                start: None,
                end: None,
            },
            before: None,
            paged: false,
            filter_id: 0,
        };
        index::read(
            &path,
            &index_path,
            &file,
            spec,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        let original = fs::read(&index_path).unwrap();
        let result = index::read(
            &path,
            &index_path,
            &file,
            ParseSpec {
                generation: 1,
                ..spec
            },
            &AtomicU64::new(0),
            &AtomicBool::new(true),
            None,
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&index_path).unwrap(), original);
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn disk_index_extends_last_record_and_keeps_prior_records() {
        let logs = viewer();
        let file = file("one");
        let path = logs.cache_root.join("source.log");
        let index_path = logs.cache_root.join("source.sqlite");
        let initial = "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:01:00.000 Error: hel";
        let extra = "lo\n stack trace\n2026-10-14 08:02:00.000 Info: last\n";
        fs::write(&path, initial).unwrap();
        let spec = ParseSpec {
            generation: 2,
            bytes: initial.len() as u64,
            offset: FixedOffset::east_opt(0).unwrap(),
            window: TimeWindow {
                start: None,
                end: None,
            },
            before: None,
            paged: false,
            filter_id: 0,
        };
        index::read(
            &path,
            &index_path,
            &file,
            spec,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(extra.as_bytes())
            .unwrap();
        let new_spec = ParseSpec {
            bytes: (initial.len() + extra.len()) as u64,
            ..spec
        };
        let (indexed, _) = index::read(
            &path,
            &index_path,
            &file,
            new_spec,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
            None,
        )
        .unwrap();
        let text = format!("{initial}{extra}");
        let (scanned, _) = presentation::Stream::read_window(
            std::io::BufReader::new(text.as_bytes()),
            &file,
            spec.offset,
            None,
            None,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(
            indexed.records.iter().map(|r| &r.raw).collect::<Vec<_>>(),
            scanned.records.iter().map(|r| &r.raw).collect::<Vec<_>>()
        );
        assert_eq!(indexed.records.len(), 3);
        assert!(indexed.records[1].raw.contains("hello\n stack trace"));
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn oversized_line_uses_background_scan_instead_of_hiding_logs() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let raw = format!(
            "2026-10-14 08:00:00.000 Info: {}\n",
            "x".repeat(4 * 1024 * 1024 + 1)
        );
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        logs.select(0);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !logs.parsed_streams.contains_key(&source.key()) && Instant::now() < deadline {
            logs.tick(&runtime, true);
            std::thread::sleep(Duration::from_millis(5));
        }
        let stream = logs
            .parsed_streams
            .get(&source.key())
            .expect("background scan did not complete");
        assert_eq!(stream.records.len(), 1);
        assert_eq!(stream.records[0].raw.len(), raw.trim_end().len());
        assert!(logs.message.contains("Index unavailable"));
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn indexed_pages_move_across_limit_and_back_without_gaps() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let raw: String = (0..10_005)
            .map(|i| {
                format!(
                    "2026-10-14 08:00:00.000 Info: item-{i:06} {}\n",
                    "x".repeat(70)
                )
            })
            .collect();
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        logs.select(0);
        let wait = |logs: &mut LogViewer| {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !logs.parse_jobs.is_empty() && Instant::now() < deadline {
                logs.tick(&runtime, true);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(logs.parse_jobs.is_empty(), "indexed page did not finish");
        };
        wait(&mut logs);
        let key = source.key();
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert!(logs.move_host_anchor_to_edge(false));
        assert!(logs.move_host_anchor(-1));
        assert!(logs.page_navigation.as_ref().unwrap().pending.is_some());
        assert_eq!(
            logs.parsed_streams[&key].index_first,
            Some(5),
            "old page remains usable during load"
        );
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(0));
        assert_eq!(logs.parsed_streams[&key].records.len(), 5);
        assert!(
            logs.parsed_streams[&key].records[4]
                .raw
                .contains("item-000004")
        );
        let appended: String = (10_005..10_010)
            .map(|i| {
                format!(
                    "2026-10-14 08:00:00.000 Info: item-{i:06} {}\n",
                    "x".repeat(70)
                )
            })
            .collect();
        logs.commit(source.clone(), false, &appended, |_| {})
            .unwrap();
        logs.rebuild_lines();
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(0));
        assert!(logs.move_host_anchor_to_edge(true));
        assert!(logs.move_host_anchor(1));
        assert!(logs.page_navigation.as_ref().unwrap().pending.is_some());
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert!(
            logs.parsed_streams[&key].records[0]
                .raw
                .contains("item-000005")
        );
        assert_eq!(logs.parsed_streams[&key].index_last, Some(10_004));
        assert!(logs.page_large_source(false));
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(10));
        assert_eq!(logs.parsed_streams[&key].index_last, Some(10_009));
        assert!(!logs.page_large_source(false));
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn indexed_content_filters_find_records_outside_the_loaded_page() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let raw: String = (0..10_005)
            .map(|i| {
                format!(
                    "2026-10-14 08:00:00.000 Info: item-{i:06} {} {}\n",
                    if i == 2 {
                        "secret"
                    } else if i == 3 {
                        "secret noisy"
                    } else {
                        "normal"
                    },
                    "x".repeat(70)
                )
            })
            .collect();
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        logs.select(0);
        logs.files_focus = false;
        let wait = |logs: &mut LogViewer| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !logs.parse_jobs.is_empty() && Instant::now() < deadline {
                logs.tick(&runtime, true);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(
                logs.parse_jobs.is_empty(),
                "indexed filtering did not finish"
            );
        };
        wait(&mut logs);
        let key = source.key();
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        let command = |logs: &mut LogViewer, c, input: &str| {
            assert!(
                logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            );
            assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)));
            for ch in input.chars() {
                logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        };
        command(&mut logs, 'i', "info");
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert!(logs.page_large_source(true));
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(0));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for ch in "secret".chars() {
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        command(&mut logs, 'o', "noisy");
        assert!(logs.index_filter_changed.is_some());
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(2));
        assert_eq!(logs.parsed_streams[&key].records.len(), 1);
        assert!(logs.visible_text().contains("item-000002"));
        assert!(!logs.visible_text().contains("item-000003"));
        assert!(logs.parsed_large[&key].0.filter_id != 0);
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for ch in "missing-term".chars() {
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert!(logs.parsed_streams[&key].records.is_empty());
        assert!(logs.indexed_filter_completed());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| crate::logs_ui::render(frame, &mut logs, true))
            .unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("No cached records match the search or filters"));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)));
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert_eq!(logs.parsed_large[&key].0.filter_id, 0);
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn indexed_filter_ignores_collector_headers_as_the_view_does() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut logs = viewer();
        logs.files_focus = false;
        let source = file("one");
        let raw = "2026-10-14 08:00:00.000 Info: visible\n==> hidden-only-marker\n2026-10-14 08:01:00.000 Info: another\n";
        let path = logs.cache_root.join("source.log");
        let index = logs.cache_root.join("source.sqlite");
        fs::write(&path, raw).unwrap();
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE)));
        for c in "hidden-only-marker".chars() {
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let spec = ParseSpec {
            generation: 0,
            bytes: raw.len() as u64,
            offset: FixedOffset::east_opt(0).unwrap(),
            window: TimeWindow {
                start: None,
                end: None,
            },
            before: None,
            paged: true,
            filter_id: 1,
        };
        let matcher = filter::IndexedQuery::from_signature(
            &logs.content_filters.signature("", &logs.options),
        )
        .unwrap();
        let (stream, _) = index::read(
            &path,
            &index,
            &source,
            spec,
            &AtomicU64::new(0),
            &AtomicBool::new(false),
            Some(&matcher),
        )
        .unwrap();
        assert!(stream.records.is_empty());
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
    #[test]
    fn indexed_search_finds_raw_and_formatted_text_outside_the_page() {
        let runtime = Runtime::new().unwrap();
        let mut logs = viewer();
        let mut source = file("one");
        let raw: String = (0..10_005)
            .map(|i| {
                format!(
                    "2026-10-14 08:00:00.000 Info: item-{i:06} {}\n{}",
                    "x".repeat(70),
                    if i == 2 { " continuation-only\n" } else { "" }
                )
            })
            .collect();
        source.size = raw.len() as u64;
        logs.files.push(source.clone());
        logs.commit(source.clone(), true, &raw, |_| {}).unwrap();
        logs.select(0);
        logs.files_focus = false;
        let wait = |logs: &mut LogViewer| {
            let deadline = Instant::now() + Duration::from_secs(30);
            while !logs.parse_jobs.is_empty() && Instant::now() < deadline {
                logs.tick(&runtime, true);
                std::thread::sleep(Duration::from_millis(2));
            }
            assert!(logs.parse_jobs.is_empty(), "indexed search did not finish");
        };
        wait(&mut logs);
        let key = source.key();
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        logs.search = "item-009999".into();
        logs.search_changed();
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        assert!(!logs.parse_jobs.is_empty());
        // A new query cancels the prior worker and must not accept its result.
        logs.search = "item-000002".into();
        logs.search_changed();
        assert!(logs.index_filter_changed.is_some());
        assert_eq!(
            logs.parsed_streams[&key].index_first,
            Some(5),
            "existing page stays visible during debounce"
        );
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(2));
        assert_eq!(logs.parsed_streams[&key].records.len(), 1);
        assert!(logs.visible_text().contains("item-000002"));
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
        for ch in "item-000002".chars() {
            logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert!(logs.parsed_streams[&key].records.is_empty());
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE)));
        assert!(logs.handle_filter_prefix(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        logs.handle_filter_dialog(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(2));
        // The ERR prefix is added by formatting and is absent from the raw Info lines.
        logs.search = "ERR".into();
        logs.search_changed();
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert_eq!(logs.parsed_streams[&key].records.len(), 10_000);
        assert!(logs.page_large_source(true));
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(0));
        assert_eq!(logs.parsed_streams[&key].records.len(), 5);
        assert_eq!(
            logs.parsed_streams[&key].records[2]
                .indexed_search_rows
                .as_deref(),
            Some(&[true, false][..])
        );
        assert!(!logs.visible_text().contains("continuation-only"));
        logs.search.clear();
        logs.search_changed();
        logs.index_filter_changed = Some(Instant::now() - Duration::from_millis(301));
        logs.tick(&runtime, true);
        wait(&mut logs);
        assert_eq!(logs.parsed_streams[&key].index_first, Some(5));
        assert_eq!(logs.parsed_large[&key].0.filter_id, 0);
        logs.cancel();
        fs::remove_dir_all(logs.cache_root).unwrap();
    }
}
