//! Desktop log catalog and disk-backed record queries. Raw generations stay authoritative.
use super::{
    LogFile,
    archive::ArchiveReader,
    hash,
    parts::{PartsReader, ReadSeek},
    presentation::{self, Options},
};
use anyhow::{Context, Result, bail};
use chrono::{FixedOffset, NaiveDateTime};
use ratatui::style::Color;
use regex::RegexBuilder;
use rusqlite::{Connection, functions::FunctionFlags, params, params_from_iter, types::Value};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufReader, Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub file: LogFile,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub end: u64,
    #[serde(default)]
    pub length: u64,
    #[serde(default)]
    pub anchor_start: u64,
    #[serde(default)]
    pub anchor: String,
    #[serde(default, skip_serializing)]
    pub text: String,
    #[serde(default)]
    pub text_bytes: u64,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub generation_path: bool,
    #[serde(default)]
    pub archive: Option<String>,
    #[serde(default)]
    pub storage: Option<String>,
    #[serde(default)]
    pub full_size: Option<u64>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub time: Option<serde_json::Value>,
}
impl Snapshot {
    pub fn complete(&self) -> bool {
        self.start == 0 && self.end == self.length && self.full_size == Some(self.text_bytes)
    }
    pub fn bytes(&self) -> u64 {
        if self.text.is_empty() {
            self.text_bytes
        } else {
            self.text.len() as u64
        }
    }
    pub fn request(&self) -> serde_json::Value {
        serde_json::json!({"path":self.file.path,"offset":self.end,"anchor_start":self.anchor_start,"anchor":self.anchor,
            "full":false,"known_size":self.file.size,"known_modified":self.file.modified,
            "complete":self.end==self.length,"preserve_full":self.start==0})
    }
    pub fn open(&self, root: &Path) -> Result<Box<dyn ReadSeek>> {
        if !self.text.is_empty() {
            return Ok(Box::new(Cursor::new(self.text.as_bytes().to_vec())));
        }
        if let Some(name) = &self.storage {
            let reader = PartsReader::open(&child(&root.join("storage"), name, ".parts")?)?;
            if reader.len() != self.text_bytes {
                bail!("Daily storage length mismatch");
            }
            return Ok(Box::new(reader));
        }
        if let Some(name) = &self.archive {
            let reader = ArchiveReader::open(&child(&root.join("archives"), name, ".gz")?);
            match reader {
                Ok(reader) if reader.raw_len() == self.text_bytes => return Ok(Box::new(reader)),
                _ => {} // A retained raw generation may still be available during archive publication.
            }
        }
        let stem = hash(&self.file.key());
        let name = if self.generation_path {
            format!("{stem}.{}.generation.log", self.generation)
        } else {
            format!("{stem}.log")
        };
        let file = File::open(child(root, &name, ".log")?)?;
        if file.metadata()?.len() < self.text_bytes {
            bail!("Cache is shorter than its committed size");
        }
        Ok(Box::new(file))
    }
}
fn child(root: &Path, name: &str, suffix: &str) -> Result<PathBuf> {
    if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name) || !name.ends_with(suffix)
    {
        bail!("Invalid cache path");
    }
    let path = root.join(name);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("Cache links are not supported");
    }
    Ok(path)
}
#[derive(Clone)]
pub struct Source {
    pub snapshot: Snapshot,
    pub root: PathBuf,
    signature: String,
}
impl Source {
    pub fn id(&self) -> String {
        hash(&self.snapshot.file.key())
    }
}
#[derive(Default)]
pub struct Catalog {
    pub sources: BTreeMap<String, Source>,
    pub warnings: Vec<String>,
}

pub fn catalog(root: &Path) -> Result<Catalog> {
    let mut catalog = Catalog::default();
    for directory in [root.to_path_buf(), root.join("desktop")] {
        if !directory.exists() {
            continue;
        }
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let result = (|| -> Result<()> {
                let bytes = fs::read(&path)?;
                let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                add_source(&mut catalog, &directory, snapshot, &bytes);
                Ok(())
            })();
            if let Err(error) = result {
                catalog
                    .warnings
                    .push(format!("{}: {error:#}", path.display()));
            }
        }
    }
    let retained = root.join("retained-history.cfg");
    if retained.exists() {
        let result = (|| -> Result<()> {
            let value: serde_json::Value = serde_json::from_slice(&fs::read(retained)?)?;
            if let Some(entries) = value["entries"].as_object() {
                for entry in entries.values() {
                    let bytes = serde_json::to_vec(&entry["snapshot"])?;
                    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
                    add_source(&mut catalog, root, snapshot, &bytes);
                }
            }
            Ok(())
        })();
        if let Err(error) = result {
            catalog
                .warnings
                .push(format!("Retained history: {error:#}"));
        }
    }
    Ok(catalog)
}
fn add_source(catalog: &mut Catalog, root: &Path, snapshot: Snapshot, metadata: &[u8]) {
    let id = hash(&snapshot.file.key());
    let updated = |snapshot: &Snapshot| {
        snapshot
            .updated_at
            .as_deref()
            .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
            .map(|time| time.timestamp_millis())
    };
    let replace = catalog.sources.get(&id).is_none_or(|old| {
        updated(&snapshot) > updated(&old.snapshot)
            || (updated(&snapshot) == updated(&old.snapshot)
                && snapshot.bytes() > old.snapshot.bytes())
    });
    if replace {
        catalog.sources.insert(
            id,
            Source {
                snapshot,
                root: root.into(),
                signature: hash(&format!(
                    "{}:{}",
                    root.display(),
                    String::from_utf8_lossy(metadata)
                )),
            },
        );
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Inventory {
    pub now: String,
    pub files: Vec<LogFile>,
    pub errors: BTreeMap<String, String>,
}
pub fn load_inventory(root: &Path) -> Result<Inventory> {
    let path = root.join("desktop/inventory.cfg");
    if !path.exists() {
        return Ok(Inventory::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn save_inventory(root: &Path, inventory: &Inventory) -> Result<()> {
    publish(
        &root.join("desktop/inventory.cfg"),
        &serde_json::to_vec(inventory)?,
    )
}
fn publish(path: &Path, bytes: &[u8]) -> Result<()> {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    fs::create_dir_all(path.parent().context("Missing cache directory")?)?;
    let temp = path.with_extension(format!(
        "{}.{}.tmp",
        std::process::id(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[derive(Debug, Deserialize)]
pub struct Chunk {
    pub host: String,
    pub path: String,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub end: u64,
    #[serde(default)]
    pub length: u64,
    #[serde(default)]
    pub reset: bool,
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub anchor_start: u64,
    #[serde(default)]
    pub anchor: String,
    #[serde(default)]
    pub missing: bool,
    #[serde(default)]
    pub error: Option<String>,
}
/// Publish an immutable generation only after every returned cursor has been validated.
pub fn save_chunk(
    root: &Path,
    file: LogFile,
    chunk: Chunk,
    old: Option<&Source>,
    cancelled: &AtomicBool,
) -> Result<()> {
    if chunk.host != file.host || chunk.path != file.path || chunk.missing || chunk.error.is_some()
    {
        bail!(
            "Invalid log download: {}",
            chunk
                .error
                .as_deref()
                .unwrap_or("missing source or mismatched host/path")
        );
    }
    if chunk.end < chunk.start
        || chunk.end > chunk.length
        || chunk.text.chars().count() as u64 != chunk.end - chunk.start
    {
        bail!("Incomplete log response; previous cache retained");
    }
    if chunk.full
        && (!chunk.reset
            || chunk.start != 0
            || chunk.end != chunk.length
            || chunk.text.len() as u64 != file.size)
    {
        bail!("Incomplete full download; previous cache retained");
    }
    if !chunk.reset && (old.is_none() || old.is_some_and(|s| s.snapshot.end != chunk.start)) {
        bail!("Log cursor changed; retry the download");
    }
    let folder = root.join("desktop");
    fs::create_dir_all(&folder)?;
    let generation = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos() as u64;
    let id = hash(&file.key());
    let path = folder.join(format!("{id}.{generation}.generation.log"));
    let result = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)?;
        let mut start = chunk.start;
        if !chunk.reset {
            if let Some(old) = old {
                let bytes = old.snapshot.bytes();
                let copied =
                    std::io::copy(&mut old.snapshot.open(&old.root)?.take(bytes), &mut output)?;
                if copied != bytes {
                    bail!("Existing cache is incomplete");
                }
                start = old.snapshot.start;
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        output.write_all(chunk.text.as_bytes())?;
        output.sync_all()?;
        let text_bytes = output.metadata()?.len();
        drop(output);
        let full_size = (start == 0 && chunk.end == chunk.length && text_bytes == file.size)
            .then_some(file.size);
        let snapshot = Snapshot {
            file,
            start,
            end: chunk.end,
            length: chunk.length,
            anchor_start: chunk.anchor_start,
            anchor: chunk.anchor,
            text: String::new(),
            text_bytes,
            generation,
            generation_path: true,
            archive: None,
            storage: None,
            full_size,
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            time: None,
        };
        if cancelled.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        publish(
            &folder.join(format!("{id}.json")),
            &serde_json::to_vec(&snapshot)?,
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    if result.is_ok() {
        if let Some(old) = old {
            // Readers already hold a bounded file handle. Unix can retire the old name;
            // on Windows a busy generation is left intact for later cleanup.
            if old.root == folder
                && old.snapshot.generation_path
                && old.snapshot.archive.is_none()
                && old.snapshot.storage.is_none()
            {
                let previous =
                    folder.join(format!("{id}.{}.generation.log", old.snapshot.generation));
                if previous != path {
                    let _ = fs::remove_file(previous);
                }
            }
        }
    }
    result
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CellScope {
    pub source: String,
    pub day: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Query {
    pub request_id: u64,
    pub merged: bool,
    pub split_hosts: bool,
    pub cells: Vec<CellScope>,
    pub files: Vec<String>,
    pub hosts: Vec<String>,
    pub search: String,
    pub include: String,
    pub exclude: String,
    pub filter_rules: Vec<super::content_filter::Rule>,
    pub errors_only: bool,
    pub start: Option<String>,
    pub end: Option<String>,
    pub offset: u64,
    pub newest: bool,
    pub raw: bool,
    pub full_errors: bool,
    pub context_before: u8,
    pub context_after: u8,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub id: String,
    pub ordinal: u64,
    pub merged_ordinal: Option<u64>,
    pub host_ordinal: Option<u64>,
    pub source: String,
    pub host: String,
    pub file: String,
    pub time: Option<String>,
    pub level: String,
    pub kind: String,
    pub port: String,
    pub method: Option<String>,
    pub text: String,
    pub lines: Option<Vec<Vec<StyledSpan>>>,
    pub raw: String,
    pub matched: bool,
}
#[derive(Serialize)]
pub struct StyledSpan {
    pub text: String,
    pub color: Option<String>,
}

fn css_color(color: Color) -> Option<String> {
    let value = match color {
        Color::Reset => return None,
        Color::Black => "#000000",
        Color::Red => "#e06c75",
        Color::Green => "#5ecb78",
        Color::Yellow => "#e5c07b",
        Color::Blue => "#65a9ed",
        Color::Magenta => "#c678dd",
        Color::Cyan => "#56b6c2",
        Color::Gray => "#abb2bf",
        Color::DarkGray => "#7f8c98",
        Color::LightRed => "#ff9898",
        Color::LightGreen => "#98e0a5",
        Color::LightYellow => "#f2dc98",
        Color::LightBlue => "#a5c8ff",
        Color::LightMagenta => "#e5aaff",
        Color::LightCyan => "#a0e1e6",
        Color::White => "#e8edf2",
        Color::Rgb(r, g, b) => return Some(format!("#{r:02x}{g:02x}{b:02x}")),
        Color::Indexed(index) => return Some(format!("var(--ansi-{index}, #e8edf2)")),
    };
    Some(value.into())
}
#[derive(Clone, Serialize)]
pub struct Bucket {
    pub time: String,
    pub count: u64,
}
#[derive(Clone, Serialize)]
pub struct HostCount {
    pub host: String,
    pub count: u64,
}
#[derive(Clone, Serialize)]
pub struct TimeBounds {
    pub first: Option<String>,
    pub last: Option<String>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub rows: Vec<Row>,
    pub total: u64,
    pub offset: u64,
    pub page_size: u64,
    pub page_total: u64,
    pub host_timeline: BTreeMap<String, Vec<Bucket>>,
    pub hosts: Vec<HostCount>,
    pub timeline: Vec<Bucket>,
    pub first: Option<String>,
    pub last: Option<String>,
    pub available_first: Option<String>,
    pub available_last: Option<String>,
    pub available_hosts: BTreeMap<String, TimeBounds>,
    pub warnings: Vec<String>,
    pub coverage: BTreeMap<String, Vec<String>>,
}

fn database(root: &Path) -> Result<Connection> {
    fs::create_dir_all(root.join("desktop"))?;
    let conn = Connection::open(root.join("desktop/records.sqlite"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA cache_size=-65536;
      CREATE TABLE IF NOT EXISTS sources(id TEXT PRIMARY KEY, signature TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS records(source TEXT NOT NULL, ordinal INTEGER NOT NULL, host TEXT NOT NULL, file TEXT NOT NULL,
        timestamp TEXT, level TEXT NOT NULL, kind TEXT NOT NULL, port TEXT NOT NULL, raw TEXT NOT NULL, PRIMARY KEY(source,ordinal));
      CREATE INDEX IF NOT EXISTS records_time ON records(timestamp,source,ordinal);
      CREATE INDEX IF NOT EXISTS records_source_time ON records(source,timestamp);
      CREATE INDEX IF NOT EXISTS records_scope ON records(source,timestamp,ordinal,host);
      CREATE INDEX IF NOT EXISTS records_days ON records(source,substr(timestamp,1,10)) WHERE timestamp IS NOT NULL;")?;
    Ok(conn)
}
fn cancelled(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Relaxed) {
        bail!("Log operation cancelled");
    }
    Ok(())
}

#[derive(Clone)]
struct Summary {
    total: u64,
    hosts: Vec<HostCount>,
    first: Option<String>,
    last: Option<String>,
    available_first: Option<String>,
    available_last: Option<String>,
    available_hosts: BTreeMap<String, TimeBounds>,
    timeline: Vec<Bucket>,
    host_timeline: BTreeMap<String, Vec<Bucket>>,
}

/// A desktop session retains its filtered record keys and summary while paging.
/// Raw log text stays in the on-disk index and only visible rows are formatted.
pub struct Reader {
    root: PathBuf,
    conn: Connection,
    prepared: Option<(String, Summary)>,
    coverage: Option<BTreeMap<String, Vec<String>>>,
}
impl Reader {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.into(),
            conn: database(root)?,
            prepared: None,
            coverage: None,
        })
    }
    pub fn page(
        &mut self,
        query: &Query,
        cancel: Arc<AtomicBool>,
        mut progress: impl FnMut(&str, u64),
    ) -> Result<Page> {
        let root = &self.root;
        if query.files.len() > 2000 || query.hosts.len() > 1000 || query.offset > i64::MAX as u64 {
            bail!("Log query is too large");
        }
        for value in [&query.start, &query.end].into_iter().flatten() {
            NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f")
                .context("Invalid log time; use server-local date and time")?;
        }
        if query
            .start
            .as_ref()
            .zip(query.end.as_ref())
            .is_some_and(|(a, b)| a > b)
        {
            bail!("Start time must precede end time");
        }
        let pattern = search_pattern(&query.search)?;
        let rules = super::content_filter::Rules::new(&query.filter_rules)?;
        let catalog = catalog(root)?;
        let conn = &mut self.conn;
        let sql_cancel = cancel.clone();
        conn.progress_handler(2000, Some(move || sql_cancel.load(Ordering::Relaxed)))?;
        for id in &query.files {
            cancelled(&cancel)?;
            let Some(source) = catalog.sources.get(id) else {
                continue;
            };
            let signature: Option<String> = conn
                .query_row("SELECT signature FROM sources WHERE id=?", [id], |row| {
                    row.get(0)
                })
                .ok();
            if signature.as_deref() == Some(&source.signature) {
                continue;
            }
            self.prepared = None;
            self.coverage = None;
            progress(&source.snapshot.file.name, 0);
            let tx = conn.transaction()?;
            tx.execute("DELETE FROM records WHERE source=?", [id])?;
            let mut ordinal = 0i64;
            let bytes = AtomicU64::new(0);
            {
                let mut insert = tx.prepare("INSERT INTO records VALUES(?,?,?,?,?,?,?,?,?)")?;
                let offset = source_offset(&source.snapshot);
                presentation::visit_records(
                    BufReader::new(
                        source
                            .snapshot
                            .open(&source.root)?
                            .take(source.snapshot.bytes()),
                    ),
                    &source.snapshot.file,
                    offset,
                    &bytes,
                    &cancel,
                    |record| {
                        ordinal += 1;
                        insert
                            .execute(params![
                                id,
                                ordinal,
                                record.host,
                                record.file,
                                record
                                    .timestamp
                                    .map(|t| t.format("%Y-%m-%dT%H:%M:%S%.3f").to_string()),
                                record.level,
                                record.kind.label(),
                                record.port,
                                record.raw
                            ])
                            .map_err(std::io::Error::other)?;
                        if ordinal % 2000 == 0 {
                            progress(&source.snapshot.file.name, bytes.load(Ordering::Relaxed));
                        }
                        Ok(())
                    },
                )?;
            }
            tx.execute(
                "INSERT OR REPLACE INTO sources VALUES(?,?)",
                params![id, source.signature],
            )?;
            cancelled(&cancel)?;
            tx.commit()?;
        }
        cancelled(&cancel)?;
        let mut scope_query = query.clone();
        scope_query.request_id = 0;
        scope_query.offset = 0;
        scope_query.newest = false;
        scope_query.raw = false;
        scope_query.full_errors = false;
        scope_query.merged = false;
        scope_query.split_hosts = false;
        let signatures: Vec<_> = query
            .files
            .iter()
            .map(|id| catalog.sources.get(id).map(|source| &source.signature))
            .collect();
        let scope_key = serde_json::to_string(&(scope_query, signatures))?;
        let cached = self
            .prepared
            .as_ref()
            .filter(|(key, _)| key == &scope_key)
            .map(|(_, summary)| summary.clone());
        let summary = if let Some(summary) = cached {
            summary
        } else {
            self.prepared = None;
            conn.create_scalar_function(
                "log_match",
                1,
                FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                move |ctx| {
                    let raw = ctx.get::<String>(0)?;
                    Ok(pattern
                        .as_ref()
                        .is_none_or(|pattern| pattern.is_match(&raw)))
                },
            )?;
            let mut clauses = if query.search.is_empty() {
                Vec::new()
            } else {
                vec!["log_match(raw)".to_owned()]
            };
            let mut values: Vec<Value> = Vec::new();
            selection(&mut clauses, &mut values, "source", &query.files);
            if !query.hosts.is_empty() {
                selection(&mut clauses, &mut values, "host", &query.hosts);
            }
            if !query.include.is_empty() || !query.exclude.is_empty() {
                let terms = super::content_filter::Terms::new(&query.include, &query.exclude);
                conn.create_scalar_function(
                    "log_filter",
                    3,
                    FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                    move |ctx| {
                        let host = ctx.get::<String>(0)?;
                        let file = ctx.get::<String>(1)?;
                        let raw = ctx.get::<String>(2)?;
                        Ok(terms.matches(&[&host, &file, &raw]))
                    },
                )?;
                clauses.push("log_filter(host,file,raw)".into());
            }
            if !query.filter_rules.is_empty() {
                conn.create_scalar_function(
                    "log_rules",
                    6,
                    FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
                    move |ctx| {
                        Ok(rules.matches(
                            &ctx.get::<String>(0)?,
                            &ctx.get::<String>(1)?,
                            &ctx.get::<String>(2)?,
                            &ctx.get::<String>(3)?,
                            &ctx.get::<String>(4)?,
                            &ctx.get::<String>(5)?,
                        ))
                    },
                )?;
                clauses.push("log_rules(host,file,raw,port,level,kind)".into());
            }
            if query.cells.len() > 2000 {
                bail!("Too many grid cells selected");
            }
            if !query.cells.is_empty() {
                let mut cells = Vec::new();
                for cell in &query.cells {
                    let day = chrono::NaiveDate::parse_from_str(&cell.day, "%Y-%m-%d")
                        .context("Invalid grid day")?;
                    let next = day.succ_opt().context("Grid day is out of range")?;
                    cells.push("(source=? AND timestamp>=? AND timestamp<?)");
                    values.push(Value::Text(cell.source.clone()));
                    values.push(Value::Text(format!("{day}T00:00:00.000")));
                    values.push(Value::Text(format!("{next}T00:00:00.000")));
                }
                clauses.push(format!("({})", cells.join(" OR ")));
            }
            if query.errors_only {
                clauses.push("level IN ('Error','Fatal','Alert','Critical')".into());
            }
            let available = if query.start.is_some() || query.end.is_some() {
                Some(conn.query_row(
                    &format!(
                        "SELECT MIN(timestamp),MAX(timestamp) FROM records WHERE {}",
                        clauses.join(" AND ")
                    ),
                    params_from_iter(values.iter()),
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<String>>(1)?,
                        ))
                    },
                )?)
            } else {
                None
            };
            let mut available_hosts = BTreeMap::new();
            {
                let sql = format!(
                    "SELECT host,MIN(timestamp),MAX(timestamp) FROM records WHERE {} GROUP BY host",
                    clauses.join(" AND ")
                );
                for result in
                    conn.prepare(&sql)?
                        .query_map(params_from_iter(values.iter()), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                TimeBounds {
                                    first: row.get(1)?,
                                    last: row.get(2)?,
                                },
                            ))
                        })?
                {
                    let (host, bounds) = result?;
                    available_hosts.insert(host, bounds);
                }
            }
            if let Some(start) = &query.start {
                clauses.push("timestamp >= ?".into());
                values.push(Value::Text(start.clone()));
            }
            if let Some(end) = &query.end {
                clauses.push("timestamp <= ?".into());
                values.push(Value::Text(end.clone()));
            }
            let matched = format!(
                "SELECT source,ordinal,host,timestamp FROM records WHERE {}",
                clauses.join(" AND ")
            );
            let before = query.context_before.min(10);
            let after = query.context_after.min(10);
            let cte = if before > 0 || after > 0 {
                // Context can relax the search, but must still obey content
                // filters, selected calendar cells, hosts and time bounds.
                let eligible = clauses
                    .iter()
                    .filter(|clause| clause.as_str() != "log_match(raw)")
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" AND ");
                values.extend(values.clone());
                format!(
                    "WITH matched AS MATERIALIZED ({matched}), visible AS (SELECT r.source,r.ordinal,r.host,r.timestamp FROM records r WHERE {eligible} AND r.source IN (SELECT source FROM matched) AND EXISTS (SELECT 1 FROM matched m WHERE m.source=r.source AND r.ordinal BETWEEN m.ordinal-{before} AND m.ordinal+{after}))"
                )
            } else {
                format!(
                    "WITH matched AS MATERIALIZED ({matched}), visible AS (SELECT * FROM matched)"
                )
            };
            let matched_flag = if before > 0 || after > 0 {
                "EXISTS(SELECT 1 FROM matched m WHERE m.source=v.source AND m.ordinal=v.ordinal)"
            } else {
                "1"
            };
            let tx = conn.transaction()?;
            tx.execute_batch("DROP TABLE IF EXISTS temp.visible_records;
        CREATE TEMP TABLE visible_records(source TEXT,ordinal INTEGER,host TEXT,timestamp TEXT,matched INTEGER);")?;
            tx.execute(&format!("{cte} INSERT INTO visible_records SELECT v.source,v.ordinal,v.host,v.timestamp,{matched_flag} FROM visible v"),params_from_iter(values.iter()))?;
            tx.execute_batch(
                "CREATE INDEX visible_time ON visible_records(timestamp,source,ordinal);
        CREATE INDEX visible_host_time ON visible_records(host,timestamp,source,ordinal);",
            )?;
            tx.commit()?;
            let cte = "WITH visible AS (SELECT * FROM visible_records)";
            let values: Vec<Value> = Vec::new();
            let total: i64 = conn.query_row(
                &format!("{cte} SELECT COUNT(*) FROM visible"),
                params_from_iter(values.iter()),
                |r| r.get(0),
            )?;
            let total = total as u64;
            let (first, last): (Option<String>, Option<String>) = conn.query_row(
                &format!("{cte} SELECT MIN(timestamp),MAX(timestamp) FROM visible"),
                params_from_iter(values.iter()),
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let mut hosts = Vec::new();
            for host in conn
                .prepare(&format!(
                    "{cte} SELECT host,COUNT(*) FROM visible GROUP BY host ORDER BY host"
                ))?
                .query_map(params_from_iter(values.iter()), |r| {
                    Ok(HostCount {
                        host: r.get(0)?,
                        count: r.get::<_, i64>(1)? as u64,
                    })
                })?
            {
                hosts.push(host?);
            }
            let mut timeline = Vec::new();
            let mut host_timeline = BTreeMap::new();
            let span = first.as_ref().zip(last.as_ref()).and_then(|(a, b)| {
                Some((
                    NaiveDateTime::parse_from_str(a, "%Y-%m-%dT%H:%M:%S%.f").ok()?,
                    NaiveDateTime::parse_from_str(b, "%Y-%m-%dT%H:%M:%S%.f").ok()?,
                ))
            });
            if let Some((start, end)) = span {
                let duration = (end - start).num_seconds().max(0) + 1;
                let seconds = ((duration + 79) / 80).max(1);
                let count = ((duration - 1) / seconds + 1) as usize;
                timeline = (0..count)
                    .map(|index| Bucket {
                        time: (start + chrono::Duration::seconds(index as i64 * seconds))
                            .format("%Y-%m-%dT%H:%M:%S%.3f")
                            .to_string(),
                        count: 0,
                    })
                    .collect();
                for host in &hosts {
                    host_timeline.insert(host.host.clone(), timeline.clone());
                }
                let sql = format!(
                    "{cte} SELECT host,CAST((strftime('%s',timestamp)-strftime('%s','{}'))/{seconds} AS INTEGER),COUNT(*) FROM visible WHERE timestamp IS NOT NULL GROUP BY host,2",
                    start.format("%Y-%m-%dT%H:%M:%S")
                );
                for bucket in
                    conn.prepare(&sql)?
                        .query_map(params_from_iter(values.iter()), |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, i64>(1)? as usize,
                                row.get::<_, i64>(2)? as u64,
                            ))
                        })?
                {
                    let (host, index, count) = bucket?;
                    if let Some(bucket) = timeline.get_mut(index) {
                        bucket.count += count;
                    }
                    if let Some(bucket) = host_timeline
                        .get_mut(&host)
                        .and_then(|bins| bins.get_mut(index))
                    {
                        bucket.count = count;
                    }
                }
            }
            let summary = Summary {
                total,
                hosts,
                available_first: available
                    .as_ref()
                    .map(|bounds| bounds.0.clone())
                    .unwrap_or_else(|| first.clone()),
                available_last: available
                    .as_ref()
                    .map(|bounds| bounds.1.clone())
                    .unwrap_or_else(|| last.clone()),
                available_hosts,
                first,
                last,
                timeline,
                host_timeline,
            };
            self.prepared = Some((scope_key, summary.clone()));
            summary
        };
        let Summary {
            total,
            hosts,
            first,
            last,
            available_first,
            available_last,
            available_hosts,
            timeline,
            host_timeline,
        } = summary;
        let page_size = if query.split_hosts { 100 } else { 200 };
        let page_total = if query.split_hosts {
            hosts.iter().map(|host| host.count).max().unwrap_or(0)
        } else {
            total
        };
        let offset = if query.newest {
            page_total.saturating_sub(page_size)
        } else {
            query.offset.min(page_total.saturating_sub(1))
        };
        let select =
            "v.source,v.ordinal,v.host,r.file,v.timestamp,r.level,r.kind,r.port,r.raw,v.matched";
        let base = format!(
            "SELECT {select} FROM visible_records v JOIN records r ON r.source=v.source AND r.ordinal=v.ordinal"
        );
        let mut requests = Vec::new();
        if query.split_hosts {
            for host in &hosts {
                let host_offset = if query.newest {
                    host.count.saturating_sub(page_size)
                } else {
                    offset
                };
                let tail = query.newest;
                let limit = page_size.min(host.count.saturating_sub(host_offset));
                let order = if tail { "DESC" } else { "ASC" };
                let skip = if tail { 0 } else { host_offset };
                requests.push((format!("{base} WHERE v.host=? ORDER BY v.timestamp {order},v.source {order},v.ordinal {order} LIMIT {limit} OFFSET {skip}"), vec![Value::Text(host.host.clone())], tail, Some((host_offset, host.count))));
            }
        } else {
            let tail = query.newest;
            let limit = page_size.min(total.saturating_sub(offset));
            let order = if tail { "DESC" } else { "ASC" };
            let skip = if tail { 0 } else { offset };
            requests.push((format!("{base} ORDER BY v.timestamp {order},v.source {order},v.ordinal {order} LIMIT {limit} OFFSET {skip}"), Vec::new(), tail, None));
        }
        let mut rows = Vec::new();
        let mut returned_bytes = 0usize;
        for (sql, values, reverse, host_position) in requests {
            let batch_start = rows.len();
            let mut statement = conn.prepare(&sql)?;
            let mut cursor = statement.query(params_from_iter(values.iter()))?;
            while let Some(row) = cursor.next()? {
                cancelled(&cancel)?;
                let source: String = row.get(0)?;
                let ordinal: i64 = row.get(1)?;
                let raw: String = row.get(8)?;
                if returned_bytes + raw.len() > 4 * 1024 * 1024 && !rows.is_empty() {
                    break;
                }
                returned_bytes += raw.len();
                let host: String = row.get(2)?;
                let file: String = row.get(3)?;
                let mut stream = presentation::Stream::default();
                let metadata = &catalog
                    .sources
                    .get(&source)
                    .context("Indexed source is unavailable")?
                    .snapshot;
                stream.update(&metadata.file, &raw, true, source_offset(metadata));
                let options = Options {
                    full_errors: query.full_errors,
                    ..Options::default()
                };
                let lines = if query.raw {
                    None
                } else {
                    Some(
                        stream
                            .records
                            .iter()
                            .flat_map(|record| record.render(&options, None, None, true))
                            .map(|line| {
                                line.spans
                                    .into_iter()
                                    .map(|span| StyledSpan {
                                        text: span.content.into_owned(),
                                        color: span.style.fg.and_then(css_color),
                                    })
                                    .collect()
                            })
                            .collect::<Vec<Vec<StyledSpan>>>(),
                    )
                };
                let text = lines.as_ref().map_or_else(
                    || raw.clone(),
                    |lines| {
                        lines
                            .iter()
                            .map(|line| {
                                line.iter()
                                    .map(|span| span.text.as_str())
                                    .collect::<String>()
                            })
                            .collect::<Vec<_>>()
                            .join("\n")
                    },
                );
                rows.push(Row {
                    id: format!("{source}:{ordinal}"),
                    ordinal: ordinal as u64,
                    merged_ordinal: None,
                    host_ordinal: None,
                    source,
                    host,
                    file,
                    time: row.get(4)?,
                    level: row.get(5)?,
                    kind: row.get(6)?,
                    port: row.get(7)?,
                    method: presentation::http_method(&raw),
                    text,
                    lines,
                    raw,
                    matched: row.get(9)?,
                });
            }
            if reverse {
                rows[batch_start..].reverse();
            }
            if let Some((host_offset, count)) = host_position {
                let base = if reverse {
                    count.saturating_sub((rows.len() - batch_start) as u64)
                } else {
                    host_offset
                };
                for (index, row) in rows[batch_start..].iter_mut().enumerate() {
                    row.host_ordinal = Some(base + index as u64 + 1);
                }
            }
        }
        let mut result_offset = offset;
        if query.merged && !query.split_hosts && query.newest {
            // A byte-limited tail fetch starts at the end, even when it returns
            // fewer records than requested.
            result_offset = total.saturating_sub(rows.len() as u64);
        }
        if query.merged && !rows.is_empty() {
            let mut sources: BTreeMap<String, (presentation::Stream, Vec<Option<Row>>)> =
                BTreeMap::new();
            for row in rows {
                let source = sources.entry(row.source.clone()).or_default();
                let metadata = &catalog.sources[&row.source].snapshot;
                let mut parsed = presentation::Stream::default();
                parsed.update(&metadata.file, &row.raw, true, source_offset(metadata));
                if let Some(mut record) = parsed.records.into_iter().next() {
                    record.timestamp = row.time.as_deref().and_then(|value| {
                        NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").ok()
                    });
                    source.0.records.push(record);
                    source.1.push(Some(row));
                }
            }
            let mut sources: Vec<_> = sources.into_values().collect();
            for (stream, _) in &mut sources {
                stream.recheck_order();
            }
            let order: Vec<_> = {
                let streams: Vec<_> = sources.iter().map(|(stream, _)| stream).collect();
                super::merge::MergedRecords::new(
                    &streams,
                    super::merge::TimeWindow::default(),
                    true,
                    [true; 4],
                )
                .collect()
            };
            rows = order
                .into_iter()
                .filter_map(|(source, index)| sources[source].1[index].take())
                .collect();
            if !query.split_hosts {
                for (index, row) in rows.iter_mut().enumerate() {
                    row.merged_ordinal = Some(result_offset + index as u64 + 1);
                }
            }
        }
        if self.coverage.is_none() {
            self.coverage = Some(indexed_days(conn)?);
        }
        let coverage = self.coverage.clone().unwrap_or_default();
        Ok(Page {
            rows,
            total,
            offset: result_offset,
            page_size,
            page_total,
            host_timeline,
            hosts,
            timeline,
            first,
            last,
            available_first,
            available_last,
            available_hosts,
            warnings: catalog.warnings,
            coverage,
        })
    }
}

pub fn page(
    root: &Path,
    query: &Query,
    cancel: Arc<AtomicBool>,
    progress: impl FnMut(&str, u64),
) -> Result<Page> {
    Reader::open(root)?.page(query, cancel, progress)
}

fn source_offset(snapshot: &Snapshot) -> FixedOffset {
    chrono::DateTime::parse_from_rfc3339(&snapshot.file.modified)
        .map(|t| *t.offset())
        .unwrap_or_else(|_| FixedOffset::east_opt(0).unwrap())
}
fn selection(clauses: &mut Vec<String>, values: &mut Vec<Value>, column: &str, items: &[String]) {
    if items.is_empty() {
        clauses.push("0".into());
        return;
    }
    clauses.push(format!(
        "{column} IN ({})",
        vec!["?"; items.len()].join(",")
    ));
    values.extend(items.iter().cloned().map(Value::Text));
}
fn search_pattern(search: &str) -> Result<Option<regex::Regex>> {
    if search.is_empty() {
        return Ok(None);
    }
    let (expression, case_sensitive) = if search.starts_with('/') {
        let end = search
            .rfind('/')
            .filter(|end| *end > 0)
            .context("Close the regular expression with / or /i")?;
        let flags = &search[end + 1..];
        if flags != "" && flags != "i" {
            bail!("Supported search flag: i");
        }
        (search[1..end].to_owned(), flags != "i")
    } else {
        (
            regex::escape(search),
            search.chars().any(char::is_uppercase),
        )
    };
    Ok(Some(
        RegexBuilder::new(&expression)
            .case_insensitive(!case_sensitive)
            .size_limit(2 * 1024 * 1024)
            .build()
            .context("Invalid search expression")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "marklogic-desktop-logs-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn file(host: &str, text: &str) -> LogFile {
        LogFile {
            host: host.into(),
            path: "/Logs/ErrorLog.txt".into(),
            name: "ErrorLog.txt".into(),
            size: text.len() as u64,
            modified: "2026-11-08T12:00:00+01:00".into(),
            present: true,
        }
    }
    fn chunk(file: &LogFile, text: &str) -> Chunk {
        Chunk {
            host: file.host.clone(),
            path: file.path.clone(),
            start: 0,
            end: text.chars().count() as u64,
            length: text.chars().count() as u64,
            reset: true,
            full: true,
            text: text.into(),
            anchor_start: 0,
            anchor: text.into(),
            missing: false,
            error: None,
        }
    }
    fn save(root: &Path, host: &str, text: &str) -> String {
        let file = file(host, text);
        let id = hash(&file.key());
        save_chunk(
            root,
            file.clone(),
            chunk(&file, text),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        id
    }
    fn run(root: &Path, query: Query) -> Page {
        page(root, &query, Arc::new(AtomicBool::new(false)), |_, _| {}).unwrap()
    }

    #[test]
    fn merges_hosts_searches_continuations_and_expands_context() {
        let temp = Temp::new();
        let a = save(
            &temp.0,
            "host-a",
            "2026-11-08 10:00:00.000 Info: before\n2026-11-08 10:00:01.000 Error: Failure\n  stack continuation\n2026-11-08 10:00:02.000 Info: after\n",
        );
        let b = save(
            &temp.0,
            "host-b",
            "2026-11-08 10:00:00.500 Info: other host\n",
        );
        let all = run(
            &temp.0,
            Query {
                files: vec![a.clone(), b.clone()],
                ..Query::default()
            },
        );
        assert_eq!(all.total, 4);
        assert_eq!(all.hosts.len(), 2);
        assert_eq!(all.rows[1].host, "host-b");
        assert!(!all.timeline.is_empty());
        let matching = run(
            &temp.0,
            Query {
                files: vec![a.clone(), b.clone()],
                search: "CONTINUATION".into(),
                ..Query::default()
            },
        );
        assert_eq!(matching.total, 0);
        let matching = run(
            &temp.0,
            Query {
                files: vec![a.clone(), b.clone()],
                search: "/continuation/i".into(),
                context_before: 1,
                context_after: 1,
                ..Query::default()
            },
        );
        assert_eq!(matching.total, 3);
        assert_eq!(matching.rows.iter().filter(|r| r.matched).count(), 1);
        assert!(matching.rows[1].raw.contains("stack continuation"));
        let bounded = run(
            &temp.0,
            Query {
                files: vec![a.clone(), b.clone()],
                search: "/continuation/i".into(),
                context_before: 1,
                context_after: 1,
                start: Some("2026-11-08T10:00:01.000".into()),
                end: Some("2026-11-08T10:00:01.000".into()),
                ..Query::default()
            },
        );
        assert_eq!(bounded.total, 1);
        assert!(bounded.rows[0].raw.contains("stack continuation"));
        let matching = run(
            &temp.0,
            Query {
                files: vec![a, b],
                hosts: vec!["host-a".into()],
                errors_only: true,
                exclude: "Failure".into(),
                ..Query::default()
            },
        );
        assert_eq!(matching.total, 0);
    }
    #[test]
    fn merged_numbers_are_chronological_across_hosts_pages_and_filters() {
        let temp = Temp::new();
        let text: String = (0..250)
            .map(|i| {
                format!(
                    "2026-11-08 10:{:02}:{:02}.000 Info: {} item {i}\n",
                    i / 60,
                    i % 60,
                    if i % 2 == 0 { "chosen" } else { "other" }
                )
            })
            .collect();
        let a = save(&temp.0, "host-a", &text);
        let b = save(&temp.0, "host-b", &text);
        let mut reader = Reader::open(&temp.0).unwrap();
        let mut query = Query {
            files: vec![a, b],
            merged: true,
            include: "chosen".into(),
            ..Query::default()
        };
        let read = |reader: &mut Reader, query: &Query| {
            reader
                .page(query, Arc::new(AtomicBool::new(false)), |_, _| {})
                .unwrap()
        };
        let first = read(&mut reader, &query);
        assert_eq!(first.total, 250);
        assert_eq!(first.rows.first().unwrap().merged_ordinal, Some(1));
        assert_eq!(first.rows.last().unwrap().merged_ordinal, Some(200));
        assert_eq!(first.rows[0].ordinal, 1);
        assert_eq!(first.rows[1].ordinal, 1);
        query.offset = 200;
        let second = read(&mut reader, &query);
        assert_eq!(second.rows.first().unwrap().merged_ordinal, Some(201));
        assert_eq!(second.rows.last().unwrap().merged_ordinal, Some(250));
        query.newest = true;
        let tail = read(&mut reader, &query);
        assert_eq!(tail.offset, 50);
        assert_eq!(tail.rows.first().unwrap().merged_ordinal, Some(51));
        assert_eq!(tail.rows.last().unwrap().merged_ordinal, Some(250));
        query.merged = false;
        query.split_hosts = true;
        assert!(
            read(&mut reader, &query)
                .rows
                .iter()
                .all(|row| row.merged_ordinal.is_none())
        );
    }

    #[test]
    fn merged_numbers_use_actual_position_for_byte_limited_tail() {
        let temp = Temp::new();
        let payload = "x".repeat(1_100_000);
        let text: String = (0..5)
            .map(|i| format!("2026-11-08 10:00:0{i}.000 Info: {payload}\n"))
            .collect();
        let id = save(&temp.0, "host-a", &text);
        let mut reader = Reader::open(&temp.0).unwrap();
        let mut query = Query {
            files: vec![id],
            merged: true,
            newest: true,
            ..Query::default()
        };
        let read = |reader: &mut Reader, query: &Query| {
            reader
                .page(query, Arc::new(AtomicBool::new(false)), |_, _| {})
                .unwrap()
        };
        let tail = read(&mut reader, &query);
        assert!(tail.rows.len() < 5);
        assert_eq!(tail.offset, 5 - tail.rows.len() as u64);
        assert_eq!(
            tail.rows.first().unwrap().merged_ordinal,
            Some(tail.offset + 1)
        );
        assert_eq!(tail.rows.last().unwrap().merged_ordinal, Some(5));
        query.newest = false;
        let first = read(&mut reader, &query);
        assert_eq!(first.offset, 0);
        assert_eq!(first.rows.first().unwrap().merged_ordinal, Some(1));
    }

    #[test]
    fn formatted_rows_preserve_tui_colors_and_raw_rows_remain_plain() {
        let temp = Temp::new();
        let id = save(
            &temp.0,
            "host-a",
            "2026-11-08 10:00:00.000 Error: Failure\n",
        );
        let mut query = Query {
            files: vec![id],
            ..Query::default()
        };
        let formatted = run(&temp.0, query.clone());
        let row = &formatted.rows[0];
        let spans = &row.lines.as_ref().unwrap()[0];
        assert!(
            spans
                .iter()
                .any(|span| span.text == "ERR" && span.color.as_deref() == Some("#ff9898"))
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "EE: " && span.color.as_deref() == Some("#ff9898"))
        );
        assert_eq!(
            spans
                .iter()
                .map(|span| span.text.as_str())
                .collect::<String>(),
            row.text
        );
        query.raw = true;
        let raw = run(&temp.0, query);
        assert!(raw.rows[0].lines.is_none());
        assert!(raw.rows[0].text.contains("Error: Failure"));
    }
    #[test]
    fn merged_access_rows_keep_status_path_and_type_colors() {
        let temp = Temp::new();
        let text = "127.0.0.1 - admin [08/Nov/2026:10:30:01 +0000] \"GET /v1/search?query=test HTTP/1.1\" 500 89\n";
        let mut access = file("host-a", text);
        access.name = "9060_AccessLog.txt".into();
        access.path = "/Logs/9060_AccessLog.txt".into();
        let id = hash(&access.key());
        save_chunk(
            &temp.0,
            access.clone(),
            chunk(&access, text),
            None,
            &AtomicBool::new(false),
        )
        .unwrap();
        let page = run(
            &temp.0,
            Query {
                files: vec![id],
                merged: true,
                ..Query::default()
            },
        );
        let spans = &page.rows[0].lines.as_ref().unwrap()[0];
        for (needle, color) in [
            ("ACC", "#98e0a5"),
            ("500 ", "#e06c75"),
            ("v1", "#a0e1e6"),
            ("query", "#e5c07b"),
        ] {
            assert!(
                spans
                    .iter()
                    .any(|span| span.text == needle && span.color.as_deref() == Some(color)),
                "missing {needle}"
            );
        }
    }
    #[test]
    fn content_filters_use_tui_terms_and_host_file_fields() {
        let temp = Temp::new();
        let id = save(
            &temp.0,
            "host-a",
            "2026-11-08 10:00:00.000 Info: alpha\n2026-11-08 10:00:01.000 Info: beta NOISE\n2026-11-08 10:00:02.000 Info: gamma Debug\n2026-11-08 10:00:03.000 Info: timeout\n",
        );
        let mut reader = Reader::open(&temp.0).unwrap();
        let mut query = Query {
            files: vec![id],
            include: "-F, host-a, missing".into(),
            exclude: "-F, debug,NOISE".into(),
            ..Query::default()
        };
        let read = |reader: &mut Reader, query: &Query| {
            reader
                .page(query, Arc::new(AtomicBool::new(false)), |_, _| {})
                .unwrap()
        };
        let filtered = read(&mut reader, &query);
        assert_eq!(filtered.total, 2);
        assert!(filtered.rows[0].raw.contains("alpha"));
        assert!(filtered.rows[1].raw.contains("timeout"));
        query.include = "-F, alpha,Timeout".into();
        query.exclude.clear();
        assert_eq!(read(&mut reader, &query).total, 1);
        query.include = " ErrorLog.txt ".into();
        assert_eq!(read(&mut reader, &query).total, 4);
        query.end = Some("2026-11-08T10:00:01.000".into());
        let bounded = read(&mut reader, &query);
        assert_eq!(bounded.total, 2);
        assert_eq!(bounded.last.as_deref(), query.end.as_deref());
        query.end = None;
        query.exclude = "-F, debug,NOISE".into();
        query.search = "alpha".into();
        query.context_after = 3;
        let context = read(&mut reader, &query);
        assert_eq!(context.total, 2);
        assert!(context.rows[0].raw.contains("alpha"));
        assert!(context.rows[1].raw.contains("timeout"));
        assert!(!context.rows[1].matched);
    }

    #[test]
    fn desktop_field_rules_use_indexed_port_and_parsed_method() {
        use super::super::content_filter::{Action, Case, Field, Mode, Rule};
        let temp = Temp::new();
        let mut ids = Vec::new();
        for port in ["8000", "18000"] {
            let text = "127.0.0.1 - admin [08/Nov/2026:10:00:00 +0000] \"GET /8000/POST HTTP/1.1\" 200 89\n127.0.0.1 - admin [08/Nov/2026:10:00:01 +0000] \"POST /GET HTTP/1.1\" 200 89\n";
            let mut file = file("host-a", text);
            file.name = format!("{port}_AccessLog.txt");
            file.path = format!("/Logs/{}", file.name);
            ids.push(hash(&file.key()));
            save_chunk(
                &temp.0,
                file.clone(),
                chunk(&file, text),
                None,
                &AtomicBool::new(false),
            )
            .unwrap();
        }
        let mut reader = Reader::open(&temp.0).unwrap();
        let query = Query {
            files: ids,
            filter_rules: vec![
                Rule {
                    id: "1".into(),
                    text: "GET".into(),
                    enabled: true,
                    action: Action::Include,
                    field: Field::Method,
                    mode: Mode::Exact,
                    case: Case::Sensitive,
                },
                Rule {
                    id: "2".into(),
                    text: "8000".into(),
                    enabled: true,
                    action: Action::Exclude,
                    field: Field::Port,
                    mode: Mode::Exact,
                    case: Case::Sensitive,
                },
            ],
            ..Query::default()
        };
        let result = reader
            .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
            .unwrap();
        assert_eq!(result.total, 1);
        assert_eq!(result.rows[0].port, "18000");
        assert_eq!(result.rows[0].method.as_deref(), Some("GET"));
        assert!(result.rows[0].raw.contains("/8000/POST"));
    }
    #[test]
    fn desktop_rules_filter_summary_context_and_pages_and_invalidate_cached_keys() {
        use super::super::content_filter::{Action, Case, Field, Mode, Rule};
        let temp = Temp::new();
        let text = (0..250)
            .map(|i| format!("2026-11-08 10:00:00.000 Info: chosen {i}\n"))
            .chain(std::iter::once(
                "2026-11-08 10:00:00.000 Info: NOISE\n".into(),
            ))
            .collect::<String>();
        let id = save(&temp.0, "host-a", &text);
        let mut reader = Reader::open(&temp.0).unwrap();
        let mut query = Query {
            files: vec![id],
            search: "chosen".into(),
            context_after: 2,
            filter_rules: vec![Rule {
                id: "1".into(),
                text: "NOISE".into(),
                enabled: true,
                action: Action::Exclude,
                field: Field::Text,
                mode: Mode::Exact,
                case: Case::Sensitive,
            }],
            ..Query::default()
        };
        let read = |reader: &mut Reader, query: &Query| {
            reader
                .page(query, Arc::new(AtomicBool::new(false)), |_, _| {})
                .unwrap()
        };
        assert_eq!(read(&mut reader, &query).total, 251); // Exact text means the whole raw record.
        query.filter_rules[0].mode = Mode::Regex;
        query.filter_rules[0].text = "NOISE$".into();
        let first = read(&mut reader, &query);
        assert_eq!(first.total, 250);
        assert_eq!(first.hosts[0].count, 250);
        assert_eq!(first.timeline.iter().map(|b| b.count).sum::<u64>(), 250);
        query.newest = true;
        let last = read(&mut reader, &query);
        assert!(last.offset > 0);
        assert!(last.rows.last().unwrap().raw.ends_with("chosen 249"));
        assert!(last.rows.iter().all(|r| !r.raw.contains("NOISE")));
        query.filter_rules[0].enabled = false;
        assert_eq!(read(&mut reader, &query).total, 251);
        query.filter_rules[0].enabled = true;
        query.filter_rules[0].text = "[".into();
        assert!(
            reader
                .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
                .is_err()
        );
    }
    #[test]
    fn session_paging_reuses_filtered_keys_without_reading_text_again() {
        let temp = Temp::new();
        let text = (0..250)
            .map(|index| {
                format!(
                    "2026-11-08 10:{:02}:{:02}.000 Info: matching entry {index}\n",
                    index / 60,
                    index % 60
                )
            })
            .collect::<String>();
        let id = save(&temp.0, "host-a", &text);
        let mut reader = Reader::open(&temp.0).unwrap();
        let mut query = Query {
            files: vec![id],
            search: "matching".into(),
            split_hosts: true,
            ..Query::default()
        };
        let first = reader
            .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
            .unwrap();
        assert_eq!(first.total, 250);
        reader
            .conn
            .create_scalar_function(
                "log_match",
                1,
                FunctionFlags::SQLITE_UTF8,
                |_| -> rusqlite::Result<bool> {
                    panic!("Paging must not reevaluate the text filter");
                },
            )
            .unwrap();
        query.newest = true;
        let tail = reader
            .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
            .unwrap();
        assert!(tail.rows.last().unwrap().raw.contains("entry 249"));
        query.newest = false;
        let start = reader
            .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
            .unwrap();
        assert_eq!(start.rows[0].ordinal, 1);
        query.search = "entry 249".into();
        let filtered = reader
            .page(&query, Arc::new(AtomicBool::new(false)), |_, _| {})
            .unwrap();
        assert_eq!(filtered.total, 1);
    }

    #[test]
    fn host_pages_keep_quiet_hosts_visible_and_activity_buckets_aligned() {
        let temp = Temp::new();
        let busy = (0..450)
            .map(|index| {
                format!(
                    "2026-11-08 10:{:02}:{:02}.000 Info: busy {index}\n",
                    index / 60,
                    index % 60
                )
            })
            .collect::<String>();
        let a = save(&temp.0, "host-a", &busy);
        let b = save(&temp.0, "host-b", "2026-11-08 10:07:29.000 Info: quiet\n");
        let first = run(
            &temp.0,
            Query {
                files: vec![a.clone(), b.clone()],
                split_hosts: true,
                ..Query::default()
            },
        );
        assert_eq!(first.total, 451);
        assert_eq!(first.page_total, 450);
        assert_eq!(first.page_size, 100);
        assert_eq!(
            first.rows.iter().filter(|row| row.host == "host-a").count(),
            100
        );
        assert_eq!(
            first.rows.iter().filter(|row| row.host == "host-b").count(),
            1
        );
        assert_eq!(
            first
                .rows
                .iter()
                .find(|row| row.host == "host-a")
                .unwrap()
                .host_ordinal,
            Some(1)
        );
        assert_eq!(
            first
                .rows
                .iter()
                .find(|row| row.host == "host-b")
                .unwrap()
                .host_ordinal,
            Some(1)
        );
        let quiet = &first.host_timeline["host-b"];
        assert_eq!(quiet.len(), first.timeline.len());
        assert_eq!(quiet.iter().map(|bin| bin.count).sum::<u64>(), 1);
        assert_eq!(quiet[0].count, 0);
        assert!(quiet.last().unwrap().count > 0);
        for (index, bucket) in first.timeline.iter().enumerate() {
            assert_eq!(quiet[index].time, bucket.time);
            assert_eq!(
                bucket.count,
                first
                    .host_timeline
                    .values()
                    .map(|bins| bins[index].count)
                    .sum::<u64>()
            );
        }
        let latest = run(
            &temp.0,
            Query {
                files: vec![a, b],
                split_hosts: true,
                newest: true,
                ..Query::default()
            },
        );
        assert_eq!(latest.offset, 350);
        assert!(latest.rows.iter().any(|row| row.raw.contains("busy 449")));
        assert!(latest.rows.iter().any(|row| row.raw.contains("quiet")));
        assert_eq!(
            latest
                .rows
                .iter()
                .find(|row| row.raw.contains("busy 449"))
                .unwrap()
                .host_ordinal,
            Some(450)
        );
        assert_eq!(
            latest
                .rows
                .iter()
                .find(|row| row.raw.contains("quiet"))
                .unwrap()
                .host_ordinal,
            Some(1)
        );
    }

    #[test]
    fn merged_pages_use_the_shared_chronological_merge_for_out_of_order_records() {
        let temp = Temp::new();
        let a = save(
            &temp.0,
            "host-a",
            "2026-11-08 10:00:03.000 Error: last\n2026-11-08 10:00:01.000 Info: first\n",
        );
        let b = save(&temp.0, "host-b", "2026-11-08 10:00:02.000 Info: middle\n");
        let merged = run(
            &temp.0,
            Query {
                files: vec![a, b],
                merged: true,
                ..Query::default()
            },
        );
        assert_eq!(merged.rows.len(), 3);
        assert!(merged.rows[0].raw.contains("first"));
        assert!(merged.rows[1].raw.contains("middle"));
        assert!(merged.rows[2].raw.contains("last"));
    }

    #[test]
    fn incomplete_or_cancelled_download_preserves_previous_generation() {
        let temp = Temp::new();
        let text = "2026-11-08 10:00:00 Info: héllo\n";
        let id = save(&temp.0, "host-a", text);
        let old = catalog(&temp.0).unwrap().sources[&id].clone();
        let mut broken = chunk(&old.snapshot.file, "bad");
        broken.end = 99;
        assert!(
            save_chunk(
                &temp.0,
                old.snapshot.file.clone(),
                broken,
                Some(&old),
                &AtomicBool::new(false)
            )
            .is_err()
        );
        let new_text = "2026-11-08 10:00:00 Info: replacement\n";
        let file = file("host-a", new_text);
        assert!(
            save_chunk(
                &temp.0,
                file.clone(),
                chunk(&file, new_text),
                Some(&old),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        let page = run(
            &temp.0,
            Query {
                files: vec![id],
                ..Query::default()
            },
        );
        assert!(page.rows[0].raw.contains("héllo"));
    }
    #[test]
    fn appends_new_records_and_reindexes_replacement_without_duplicates() {
        let temp = Temp::new();
        let initial = "2026-11-08 10:00:00 Info: first\n";
        let id = save(&temp.0, "host-a", initial);
        let query = Query {
            files: vec![id.clone()],
            ..Query::default()
        };
        assert_eq!(run(&temp.0, query.clone()).total, 1);
        let old = catalog(&temp.0).unwrap().sources[&id].clone();
        let old_path = old
            .root
            .join(format!("{id}.{}.generation.log", old.snapshot.generation));
        let mut existing_reader = old.snapshot.open(&old.root).unwrap();
        let append = "2026-11-08 10:00:01 Error: second\n";
        let full = format!("{initial}{append}");
        let file = file("host-a", &full);
        let mut chunk = chunk(&file, append);
        chunk.start = initial.chars().count() as u64;
        chunk.end = full.chars().count() as u64;
        chunk.length = chunk.end;
        chunk.full = false;
        chunk.reset = false;
        save_chunk(&temp.0, file, chunk, Some(&old), &AtomicBool::new(false)).unwrap();
        assert_eq!(run(&temp.0, query.clone()).total, 2);
        assert_eq!(run(&temp.0, query).total, 2);
        let old = catalog(&temp.0).unwrap().sources[&id].clone();
        assert!(old.snapshot.complete());
        #[cfg(unix)]
        assert!(!old_path.exists());
        let mut previous = String::new();
        existing_reader.read_to_string(&mut previous).unwrap();
        assert_eq!(previous, initial);
    }
    #[test]
    fn reads_committed_prefix_of_legacy_partitioned_and_archived_tui_caches() {
        let temp = Temp::new();
        let text = "2026-11-08 10:00:00 Info: legacy\n";
        let file = file("legacy", text);
        let id = hash(&file.key());
        let metadata = Snapshot {
            file: file.clone(),
            start: 0,
            end: text.len() as u64,
            length: text.len() as u64,
            anchor_start: 0,
            anchor: String::new(),
            text: String::new(),
            text_bytes: text.len() as u64,
            generation: 1,
            generation_path: false,
            archive: None,
            storage: None,
            full_size: Some(text.len() as u64),
            updated_at: None,
            time: None,
        };
        fs::write(
            temp.0.join(format!("{id}.log")),
            format!("{text}uncommitted tail"),
        )
        .unwrap();
        fs::write(
            temp.0.join(format!("{id}.json")),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        assert_eq!(
            run(
                &temp.0,
                Query {
                    files: vec![id.clone()],
                    ..Query::default()
                }
            )
            .total,
            1
        );
        fs::create_dir_all(temp.0.join("archives")).unwrap();
        super::super::archive::build(
            &temp.0.join(format!("{id}.log")),
            &temp.0.join("archives/log.gz"),
            text.len() as u64,
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut archived = metadata.clone();
        archived.archive = Some("log.gz".into());
        fs::write(
            temp.0.join(format!("{id}.json")),
            serde_json::to_vec(&archived).unwrap(),
        )
        .unwrap();
        assert_eq!(
            run(
                &temp.0,
                Query {
                    files: vec![id.clone()],
                    ..Query::default()
                }
            )
            .total,
            1
        );
        fs::create_dir_all(temp.0.join("storage")).unwrap();
        fs::write(temp.0.join("storage/part.log"), text).unwrap();
        fs::write(
            temp.0.join("storage/log.parts"),
            serde_json::to_vec(&super::super::parts::Manifest {
                parts: vec![super::super::parts::Part {
                    day: "2026-11-08".into(),
                    path: "part.log".into(),
                    bytes: text.len() as u64,
                    frozen: false,
                }],
            })
            .unwrap(),
        )
        .unwrap();
        archived.archive = None;
        archived.storage = Some("log.parts".into());
        fs::write(
            temp.0.join(format!("{id}.json")),
            serde_json::to_vec(&archived).unwrap(),
        )
        .unwrap();
        assert_eq!(
            run(
                &temp.0,
                Query {
                    files: vec![id],
                    ..Query::default()
                }
            )
            .total,
            1
        );
        assert!(child(&temp.0, "../outside.gz", ".gz").is_err());
    }
    #[test]
    fn grid_cells_filter_calendar_days_inside_one_physical_file() {
        let temp = Temp::new();
        let id = save(
            &temp.0,
            "host-a",
            "2026-11-07 23:59:59 Info: yesterday\n2026-11-08 00:00:00 Info: today\n",
        );
        let one = run(
            &temp.0,
            Query {
                files: vec![id.clone()],
                cells: vec![CellScope {
                    source: id.clone(),
                    day: "2026-11-07".into(),
                }],
                search: "yesterday".into(),
                context_after: 1,
                ..Query::default()
            },
        );
        assert_eq!(one.total, 1);
        assert!(one.rows[0].raw.contains("yesterday"));
        assert_eq!(one.coverage[&id], vec!["2026-11-07", "2026-11-08"]);
        let two = run(
            &temp.0,
            Query {
                files: vec![id.clone()],
                cells: vec![CellScope {
                    source: id,
                    day: "2026-11-08".into(),
                }],
                search: "today".into(),
                context_before: 1,
                ..Query::default()
            },
        );
        assert_eq!(two.total, 1);
        assert!(two.rows[0].raw.contains("today"));
    }

    #[test]
    fn pages_newest_records_and_rejects_invalid_queries_and_cancelled_work() {
        let temp = Temp::new();
        let text = (0..450)
            .map(|n| {
                format!(
                    "2026-11-08 10:{:02}:{:02}.000 Info: row {n}\n",
                    n / 60,
                    n % 60
                )
            })
            .collect::<String>();
        let id = save(&temp.0, "host-a", &text);
        let query = Query {
            files: vec![id],
            newest: true,
            ..Query::default()
        };
        let last = run(&temp.0, query.clone());
        assert_eq!(last.total, 450);
        assert_eq!(last.rows.len(), 200);
        assert_eq!(last.offset, 250);
        assert!(page(&temp.0, &query, Arc::new(AtomicBool::new(true)), |_, _| {}).is_err());
        assert!(
            page(
                &temp.0,
                &Query {
                    search: "/[/".into(),
                    ..query
                },
                Arc::new(AtomicBool::new(false)),
                |_, _| {}
            )
            .is_err()
        );
    }
}

/// Calendar dates already known to the record index; this never reads raw logs.
pub fn cached_days(root: &Path) -> Result<BTreeMap<String, Vec<String>>> {
    let path = root.join("desktop/records.sqlite");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(2))?;
    indexed_days(&conn)
}
fn indexed_days(conn: &Connection) -> Result<BTreeMap<String, Vec<String>>> {
    let mut result: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut stmt=conn.prepare("SELECT source,substr(timestamp,1,10) FROM records WHERE timestamp IS NOT NULL GROUP BY source,substr(timestamp,1,10) ORDER BY source,substr(timestamp,1,10)")?;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (id, day) = row?;
        result.entry(id).or_default().push(day);
    }
    Ok(result)
}

/// Reuse the TUI's existing record index or partition manifest to populate calendar cells.
pub fn source_days(source: &Source) -> Vec<String> {
    if let Some(name) = &source.snapshot.storage {
        if let Ok(path) = child(&source.root.join("storage"), name, ".parts") {
            if let Ok(manifest) = super::parts::Manifest::load(&path) {
                let mut days: Vec<_> = manifest
                    .parts
                    .into_iter()
                    .map(|part| part.day)
                    .filter(|day| chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok())
                    .collect();
                days.sort();
                days.dedup();
                return days;
            }
        }
    }
    let path = source
        .snapshot
        .archive
        .as_ref()
        .and_then(|name| child(&source.root.join("archives"), name, ".gz").ok())
        .map(|path| path.with_extension("records.sqlite"))
        .unwrap_or_else(|| {
            source.root.join(format!(
                "{}.records.sqlite",
                hash(&source.snapshot.file.key())
            ))
        });
    let read = (|| -> Result<Vec<String>> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let (generation, bytes): (i64, i64) =
            conn.query_row("SELECT generation,bytes FROM metadata", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
        if generation as u64 != source.snapshot.generation
            || bytes as u64 != source.snapshot.bytes()
        {
            return Ok(Vec::new());
        }
        let mut query = conn.prepare(
            "SELECT DISTINCT substr(time,1,10) FROM records WHERE time IS NOT NULL ORDER BY time",
        )?;
        Ok(query
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<_, _>>()?)
    })();
    read.unwrap_or_default()
}
