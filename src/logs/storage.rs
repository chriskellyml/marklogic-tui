//! Daily storage partitions for cached streams. Cursor metadata still describes
//! the remote source; the ordered parts describe only the retained local bytes.
use super::{Snapshot, archive, atomic_write, hash_key, index, presentation};
use anyhow::{Context, Result, bail};
use chrono::FixedOffset;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

pub(crate) type CellKey = (String, String); // port, ISO date (or Unknown)
const MAX_LINE: u64 = 4 * 1024 * 1024;
static NONCE: AtomicU64 = AtomicU64::new(0);

/// Overall work is measured in committed, uncompressed source bytes. Cached
/// indexes and daily manifests count as accounted for without rereading them.
#[derive(Clone, Debug, Default)]
pub(crate) struct ScanProgress {
    pub completed_sources: usize,
    pub total_sources: usize,
    pub failed_sources: usize,
    pub accounted_bytes: u64,
    pub total_bytes: u64,
    pub current: Option<ScanFileProgress>,
}

#[derive(Clone, Debug)]
pub(crate) struct ScanFileProgress {
    pub host: String,
    pub name: String,
    pub read_bytes: u64,
    pub total_bytes: u64,
}
fn nonce() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        NONCE.fetch_add(1, Ordering::Relaxed)
    )
}
pub(super) fn save(path: &Path, bytes: &[u8]) -> Result<bool> {
    let stage = path.with_extension(format!("{}.tmp", nonce()));
    let result = (|| -> Result<bool> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stage)?;
        output.write_all(bytes)?;
        output.sync_all()?;
        drop(output);
        fs::rename(&stage, path)?;
        #[cfg(unix)]
        {
            Ok(File::open(path.parent().context("No storage directory")?)
                .and_then(|directory| directory.sync_all())
                .is_ok())
        }
        #[cfg(not(unix))]
        {
            Ok(true)
        }
    })();
    let _ = fs::remove_file(stage);
    result
}

fn local_path(root: &Path, name: &str) -> io::Result<PathBuf> {
    let path = Path::new(name);
    if path.file_name().and_then(|n| n.to_str()) != Some(name) || name == "." || name == ".." {
        return Err(io::Error::other("Invalid storage filename"));
    }
    let path = root.join(path);
    if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("Storage links are not supported"));
    }
    Ok(path)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Part {
    pub day: String,
    pub path: String,
    pub bytes: u64,
    pub frozen: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Manifest {
    pub parts: Vec<Part>,
}
impl Manifest {
    pub fn load(path: &Path) -> io::Result<Self> {
        let result: Self = serde_json::from_reader(File::open(path)?).map_err(io::Error::other)?;
        for part in &result.parts {
            local_path(path.parent().unwrap(), &part.path)?;
        }
        Ok(result)
    }
    pub fn bytes(&self) -> u64 {
        self.parts.iter().map(|p| p.bytes).sum()
    }
}

pub(super) struct PartsReader {
    inputs: Vec<(u64, Box<dyn index::ReadSeek>)>,
    position: u64,
    bytes: u64,
}
impl PartsReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let manifest = Manifest::load(path)?;
        let mut inputs = Vec::new();
        let mut bytes = 0u64;
        // Open before publication can retire the previous generation. Existing
        // readers retain their handles when the files are unlinked (Unix).
        for part in manifest.parts {
            let path = local_path(path.parent().unwrap(), &part.path)?;
            let input: Box<dyn index::ReadSeek> = if part.frozen {
                let reader = archive::ArchiveReader::open(&path)?;
                if reader.raw_len() != part.bytes {
                    return Err(io::Error::other("Frozen part has the wrong length"));
                }
                Box::new(reader)
            } else {
                let file = File::open(path)?;
                if file.metadata()?.len() < part.bytes {
                    return Err(io::Error::other(
                        "Thawed part is shorter than its committed length",
                    ));
                }
                Box::new(file)
            };
            bytes = bytes
                .checked_add(part.bytes)
                .ok_or_else(|| io::Error::other("Storage length overflow"))?;
            inputs.push((bytes, input));
        }
        Ok(Self {
            inputs,
            position: 0,
            bytes,
        })
    }
    pub fn len(&self) -> u64 {
        self.bytes
    }
}
impl Read for PartsReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.bytes || output.is_empty() {
            return Ok(0);
        }
        let index = self
            .inputs
            .partition_point(|(end, _)| *end <= self.position);
        let start = if index == 0 {
            0
        } else {
            self.inputs[index - 1].0
        };
        let (end, reader) = &mut self.inputs[index];
        reader.seek(SeekFrom::Start(self.position - start))?;
        let count = output.len().min((*end - self.position) as usize);
        let count = reader.read(&mut output[..count])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete storage part",
            ));
        }
        self.position += count as u64;
        Ok(count)
    }
}
impl Seek for PartsReader {
    fn seek(&mut self, seek: SeekFrom) -> io::Result<u64> {
        let next = match seek {
            SeekFrom::Start(p) => p as i128,
            SeekFrom::End(p) => self.bytes as i128 + p as i128,
            SeekFrom::Current(p) => self.position as i128 + p as i128,
        };
        self.position = u64::try_from(next)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid seek"))?;
        Ok(self.position)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Range {
    pub day: String,
    pub offset: u64,
    pub bytes: u64,
    pub frozen: bool,
    pub stored: u64,
    pub estimated: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct CachedScan {
    signature: String,
    ranges: Vec<Range>,
}
#[derive(Clone, Debug)]
pub(crate) struct Source {
    pub root: PathBuf,
    pub metadata: PathBuf,
    pub port: String,
    pub host: String,
    pub name: String,
    pub(crate) ranges: Vec<Range>,
}
#[derive(Clone, Debug, Default)]
pub(crate) struct Inventory {
    pub sources: Vec<Source>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Size {
    pub thawed: u64,
    pub frozen: u64,
    pub logical: u64,
    pub estimated: bool,
}
impl Size {
    pub fn total(self) -> u64 {
        self.thawed.saturating_add(self.frozen)
    }
    pub fn add(&mut self, other: Self) {
        self.thawed = self.thawed.saturating_add(other.thawed);
        self.frozen = self.frozen.saturating_add(other.frozen);
        self.logical = self.logical.saturating_add(other.logical);
        self.estimated |= other.estimated;
    }
}
impl Inventory {
    pub fn cells(&self) -> std::collections::BTreeMap<CellKey, Size> {
        let mut cells = std::collections::BTreeMap::<CellKey, Size>::new();
        for source in &self.sources {
            for range in &source.ranges {
                cells
                    .entry((source.port.clone(), range.day.clone()))
                    .or_default()
                    .add(Size {
                        thawed: if range.frozen { 0 } else { range.stored },
                        frozen: if range.frozen { range.stored } else { 0 },
                        logical: range.bytes,
                        estimated: range.estimated,
                    });
            }
        }
        cells
    }
}

fn ranges(
    snapshot: &Snapshot,
    root: &Path,
    signature: &str,
    progress: &mut impl FnMut(u64, u64),
) -> Result<Vec<Range>> {
    if let Some(path) = snapshot.storage_path(root) {
        let manifest = Manifest::load(&path)?;
        let mut offset = 0;
        let mut ranges = Vec::new();
        for part in manifest.parts {
            let disk = fs::metadata(local_path(path.parent().unwrap(), &part.path)?)?.len();
            if !part.frozen && disk < part.bytes {
                bail!("Thawed part is shorter than its committed length");
            }
            let stored = if part.frozen { disk } else { part.bytes };
            ranges.push(Range {
                day: part.day,
                offset,
                bytes: part.bytes,
                frozen: part.frozen,
                stored,
                estimated: false,
            });
            offset += part.bytes;
        }
        if offset != snapshot.text_bytes {
            bail!("Daily storage length does not match the cursor");
        }
        return Ok(ranges);
    }
    let scan_path = root
        .join("storage")
        .join(format!("{}.scan.json", hash_key(&snapshot.file.key())));
    if let Ok(input) = File::open(&scan_path)
        && let Ok(cached) = serde_json::from_reader::<_, CachedScan>(input)
        && cached.signature == signature
    {
        return Ok(cached.ranges);
    }
    let bytes = if snapshot.text.is_empty() {
        snapshot.text_bytes
    } else {
        snapshot.text.len() as u64
    };
    let input: Box<dyn Read> = if snapshot.text.is_empty() {
        Box::new(snapshot.open_content(root)?.take(bytes))
    } else {
        Box::new(io::Cursor::new(snapshot.text.clone().into_bytes()))
    };
    let mut input = BufReader::new(input);
    let mut output: Vec<Range> = Vec::new();
    let mut line = Vec::new();
    let mut offset = 0u64;
    let mut previous = None;
    let mut last_progress = 0u64;
    let mut last_update = std::time::Instant::now();
    loop {
        line.clear();
        let count = input
            .by_ref()
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_LINE {
            bail!("Log line exceeds the 4 MiB storage reader limit");
        }
        let raw = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
        let (time, starts, _) =
            presentation::line_index_info(&snapshot.file, raw, FixedOffset::east_opt(0).unwrap());
        let date = if !starts || raw.trim().is_empty() {
            previous
                .clone()
                .or_else(|| time.map(|t| t.date().to_string()))
        } else {
            time.map(|t| t.date().to_string())
        };
        previous = date.clone();
        let day = date.unwrap_or_else(|| "Unknown".into());
        if let Some(range) = output.last_mut().filter(|range| range.day == day) {
            range.bytes += count as u64;
            range.stored += count as u64;
        } else {
            output.push(Range {
                day,
                offset,
                bytes: count as u64,
                stored: count as u64,
                frozen: false,
                estimated: false,
            });
        }
        offset += count as u64;
        if offset - last_progress >= 1024 * 1024 {
            last_progress = offset;
            if last_update.elapsed() >= std::time::Duration::from_millis(200) {
                progress(offset, bytes);
                last_update = std::time::Instant::now();
            }
        }
    }
    if offset != bytes {
        bail!("Incomplete committed log content");
    }
    if snapshot.archive.is_some() && bytes > 0 {
        let stored = fs::metadata(snapshot.read_path(root))?.len();
        let mut assigned = 0;
        let count = output.len();
        for (i, range) in output.iter_mut().enumerate() {
            let size = if i + 1 == count {
                stored - assigned
            } else {
                ((stored as u128 * range.bytes as u128) / bytes as u128) as u64
            };
            range.stored = size;
            range.frozen = true;
            range.estimated = count > 1;
            assigned += size;
        }
    }
    fs::create_dir_all(scan_path.parent().unwrap())?;
    atomic_write(
        &scan_path,
        &serde_json::to_vec(&CachedScan {
            signature: signature.into(),
            ranges: output.clone(),
        })?,
    )?;
    Ok(output)
}

struct ScanCandidate {
    root: PathBuf,
    metadata: PathBuf,
    bytes: u64,
}

fn snapshot_bytes(snapshot: &Snapshot) -> u64 {
    if snapshot.text.is_empty() {
        snapshot.text_bytes
    } else {
        snapshot.text.len() as u64
    }
}

fn read_candidate(path: &Path) -> Result<Option<(Snapshot, String)>> {
    let data = fs::read(path)?;
    let snapshot: Snapshot = match serde_json::from_slice(&data) {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    if path.file_name().and_then(|name| name.to_str())
        != Some(&format!("{}.json", hash_key(&snapshot.file.key())))
    {
        return Ok(None);
    }
    Ok(Some((snapshot, hash_key(&String::from_utf8_lossy(&data)))))
}

pub(crate) fn scan(root: &Path, mut progress: impl FnMut(ScanProgress)) -> Inventory {
    let mut inventory = Inventory::default();
    let mut candidates = Vec::new();
    let namespaces = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return inventory,
        Err(error) => {
            inventory.warnings.push(error.to_string());
            return inventory;
        }
    };
    for namespace in namespaces {
        let namespace = match namespace {
            Ok(value) => value,
            Err(error) => {
                inventory.warnings.push(error.to_string());
                continue;
            }
        };
        if !namespace
            .file_type()
            .is_ok_and(|t| t.is_dir() && !t.is_symlink())
        {
            continue;
        }
        let root = namespace.path();
        let entries = match fs::read_dir(&root) {
            Ok(value) => value,
            Err(error) => {
                inventory.warnings.push(error.to_string());
                continue;
            }
        };
        for entry in entries {
            let result = (|| -> Result<Option<ScanCandidate>> {
                let entry = entry?;
                if !entry.file_type()?.is_file()
                    || entry.path().extension().and_then(|s| s.to_str()) != Some("json")
                {
                    return Ok(None);
                }
                let Some((snapshot, _)) = read_candidate(&entry.path())? else {
                    return Ok(None);
                };
                Ok(Some(ScanCandidate {
                    root: root.clone(),
                    metadata: entry.path(),
                    bytes: snapshot_bytes(&snapshot),
                }))
            })();
            match result {
                Ok(Some(candidate)) => candidates.push(candidate),
                Ok(None) => {}
                Err(error) => inventory
                    .warnings
                    .push(format!("{}: {error:#}", root.display())),
            }
        }
    }
    candidates.sort_by(|a, b| a.metadata.cmp(&b.metadata));
    let mut state = ScanProgress {
        total_sources: candidates.len(),
        total_bytes: candidates.iter().fold(0u64, |total, candidate| {
            total.saturating_add(candidate.bytes)
        }),
        ..ScanProgress::default()
    };
    progress(state.clone());
    let mut completed_bytes = 0u64;
    for candidate in candidates {
        let mut source_bytes = candidate.bytes;
        let result = (|| -> Result<Source> {
            let (snapshot, signature) = read_candidate(&candidate.metadata)?
                .context("Cached source metadata changed or disappeared")?;
            source_bytes = snapshot_bytes(&snapshot);
            state.total_bytes = state
                .total_bytes
                .saturating_sub(candidate.bytes)
                .saturating_add(source_bytes);
            state.current = Some(ScanFileProgress {
                host: snapshot.file.host.clone(),
                name: snapshot.file.name.clone(),
                read_bytes: 0,
                total_bytes: source_bytes,
            });
            progress(state.clone());
            let ranges = ranges(
                &snapshot,
                &candidate.root,
                &signature,
                &mut |read, total| {
                    if let Some(current) = &mut state.current {
                        current.read_bytes = read;
                        current.total_bytes = total;
                    }
                    state.accounted_bytes = completed_bytes.saturating_add(read);
                    progress(state.clone());
                },
            )?;
            Ok(Source {
                root: candidate.root.clone(),
                metadata: candidate.metadata.clone(),
                port: super::LogViewer::file_port(&snapshot.file)
                    .unwrap_or("unknown")
                    .into(),
                host: snapshot.file.host,
                name: snapshot.file.name,
                ranges,
            })
        })();
        match result {
            Ok(source) => inventory.sources.push(source),
            Err(error) => {
                state.failed_sources += 1;
                inventory
                    .warnings
                    .push(format!("{}: {error:#}", candidate.metadata.display()));
            }
        }
        completed_bytes = completed_bytes.saturating_add(source_bytes);
        state.completed_sources += 1;
        state.accounted_bytes = completed_bytes;
        state.current = None;
        progress(state.clone());
    }
    inventory
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Delete,
    Archive,
    Thaw,
}
impl Action {
    pub fn label(self) -> &'static str {
        match self {
            Self::Delete => "Delete",
            Self::Archive => "Archive",
            Self::Thaw => "Thaw",
        }
    }
}

struct Created {
    paths: Vec<PathBuf>,
    published: bool,
}
impl Drop for Created {
    fn drop(&mut self) {
        if !self.published {
            for path in &self.paths {
                let _ = fs::remove_file(path);
            }
        }
    }
}
fn lock(root: &Path) -> Result<super::ArchivePublishLock> {
    super::ArchivePublishLock::acquire(root)
}
fn write_part(
    root: &Path,
    day: String,
    bytes: u64,
    frozen: bool,
    input: &mut dyn Read,
    created: &mut Created,
) -> Result<Part> {
    let name = format!("{}.log", nonce());
    let path = root.join(&name);
    created.paths.push(path.clone());
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)?;
    let count = io::copy(&mut input.take(bytes), &mut file)?;
    if count != bytes {
        bail!("Daily partition is shorter than its committed length");
    }
    file.sync_all()?;
    drop(file);
    if frozen {
        let gz_name = format!("{name}.gz");
        let gz = root.join(&gz_name);
        created.paths.push(gz.clone());
        created
            .paths
            .push(root.join(format!("{gz_name}.index.json")));
        archive::build(&path, &gz, bytes, &AtomicBool::new(false))?;
        fs::remove_file(path)?;
        Ok(Part {
            day,
            path: gz_name,
            bytes,
            frozen,
        })
    } else {
        Ok(Part {
            day,
            path: name,
            bytes,
            frozen,
        })
    }
}
fn retire(path: &Path, warnings: &mut Vec<String>) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => warnings.push(format!("{}: {error}", path.display())),
    }
}

/// Change each source atomically. A failure leaves that source's old manifest
/// authoritative; successful earlier sources are reported and retained.
pub(crate) fn apply(
    inventory: &Inventory,
    selected: &BTreeSet<CellKey>,
    action: Action,
    mut progress: impl FnMut(String),
) -> (usize, Vec<String>) {
    let mut changed = 0;
    let mut warnings = Vec::new();
    for source in &inventory.sources {
        if !source
            .ranges
            .iter()
            .any(|range| selected.contains(&(source.port.clone(), range.day.clone())))
        {
            continue;
        }
        let result = (|| -> Result<bool> {
            let _lock = lock(&source.root)?;
            let data = fs::read(&source.metadata)?;
            let snapshot: Snapshot = serde_json::from_slice(&data)?;
            let ranges = ranges(
                &snapshot,
                &source.root,
                &hash_key(&String::from_utf8_lossy(&data)),
                &mut |read, total| {
                    progress(format!(
                        "Indexing {} / {}: {} of {}",
                        source.host,
                        source.name,
                        super::format_bytes(read),
                        super::format_bytes(total)
                    ));
                },
            )?;
            let relevant =
                |range: &Range| selected.contains(&(source.port.clone(), range.day.clone()));
            if !ranges.iter().any(|range| {
                relevant(range)
                    && match action {
                        Action::Delete => true,
                        Action::Archive => !range.frozen,
                        Action::Thaw => range.frozen,
                    }
            }) {
                return Ok(false);
            }
            progress(format!(
                "{}: {} / {}",
                action.label(),
                source.host,
                source.name
            ));
            let storage_root = source.root.join("storage");
            fs::create_dir_all(&storage_root)?;
            let previous_manifest = snapshot
                .storage_path(&source.root)
                .map(|path| Manifest::load(&path))
                .transpose()?;
            let mut input = snapshot.open_content(&source.root)?;
            let mut created = Created {
                paths: Vec::new(),
                published: false,
            };
            let mut manifest = Manifest::default();
            for (i, range) in ranges.iter().enumerate() {
                let selected = relevant(range);
                if selected && action == Action::Delete {
                    continue;
                }
                let frozen = if selected {
                    match action {
                        Action::Archive => true,
                        Action::Thaw => false,
                        Action::Delete => unreachable!(),
                    }
                } else {
                    range.frozen
                };
                if let Some(existing) = previous_manifest
                    .as_ref()
                    .and_then(|manifest| manifest.parts.get(i))
                    .filter(|part| part.frozen == frozen)
                {
                    manifest.parts.push(existing.clone());
                } else {
                    input.seek(SeekFrom::Start(range.offset))?;
                    manifest.parts.push(write_part(
                        &storage_root,
                        range.day.clone(),
                        range.bytes,
                        frozen,
                        &mut *input,
                        &mut created,
                    )?);
                }
            }
            let name = format!("{}.parts", nonce());
            let path = storage_root.join(&name);
            created.paths.push(path.clone());
            if !save(&path, &serde_json::to_vec(&manifest)?)? {
                bail!("Could not make the new daily storage durable; originals retained");
            }
            // Verify the virtual source before publishing its cursor, including
            // file descriptor limits. A failure leaves the originals untouched.
            let verified = PartsReader::open(&path)?;
            if verified.len() != manifest.bytes() {
                bail!("Daily storage verification failed");
            }
            drop(verified);
            let mut meta = snapshot.meta();
            meta.storage = Some(name);
            meta.archive = None;
            meta.text_bytes = manifest.bytes();
            if action == Action::Delete {
                meta.generation = meta
                    .generation
                    .checked_add(1)
                    .context("Generation overflow")?;
                meta.full_size = None;
            }
            // Keep the remote position and time cursor: deletion must not
            // re-download the selected local records on the next live poll.
            if fs::read(&source.metadata)? != data {
                bail!("The cached source changed; refresh and retry");
            }
            let durable = save(&source.metadata, &serde_json::to_vec(&meta)?)?;
            created.published = true;
            if !durable {
                warnings.push(format!(
                    "{} / {}: new state published; originals retained until metadata is durable",
                    source.host, source.name
                ));
                return Ok(true);
            }
            drop(input);
            let retained: BTreeSet<_> = manifest
                .parts
                .iter()
                .map(|part| part.path.as_str())
                .collect();
            if let Some(previous) = previous_manifest {
                for part in previous.parts {
                    if !retained.contains(part.path.as_str()) {
                        let path = local_path(&storage_root, &part.path)?;
                        retire(&path, &mut warnings);
                        if part.frozen {
                            retire(
                                &storage_root.join(format!("{}.index.json", part.path)),
                                &mut warnings,
                            );
                        }
                    }
                }
                if let Some(path) = snapshot.storage_path(&source.root) {
                    retire(&path, &mut warnings);
                }
            } else {
                retire(&snapshot.content_path(&source.root), &mut warnings);
                if let Some(path) = snapshot.archive_path(&source.root) {
                    retire(&path, &mut warnings);
                    let mut sidecar = path.as_os_str().to_os_string();
                    sidecar.push(".index.json");
                    retire(Path::new(&sidecar), &mut warnings);
                }
                // Retire aliases of this exact raw generation too; otherwise
                // their hard links would keep all of the old disk blocks alive.
                let alias = source.root.join(format!(
                    "{}.{}.previous.log",
                    hash_key(&snapshot.file.key()),
                    snapshot.generation
                ));
                retire(&alias, &mut warnings);
            }
            Ok(true)
        })();
        match result {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(error) => warnings.push(format!("{} / {}: {error:#}", source.host, source.name)),
        }
    }
    (changed, warnings)
}

/// A full replacement supersedes its previous daily layout. Retire it only
/// after the replacement cursor is durable; existing readers keep open handles.
pub(super) fn retire_managed(root: &Path, snapshot: &Snapshot) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(path) = snapshot.storage_path(root) else {
        return warnings;
    };
    match Manifest::load(&path) {
        Ok(manifest) => {
            for part in manifest.parts {
                match local_path(path.parent().unwrap(), &part.path) {
                    Ok(part_path) => {
                        retire(&part_path, &mut warnings);
                        if part.frozen {
                            retire(
                                &path
                                    .parent()
                                    .unwrap()
                                    .join(format!("{}.index.json", part.path)),
                                &mut warnings,
                            );
                        }
                    }
                    Err(error) => warnings.push(error.to_string()),
                }
            }
            retire(&path, &mut warnings);
        }
        Err(error) => warnings.push(error.to_string()),
    }
    warnings
}

/// Append new data as thawed parts without expanding existing frozen days.
/// The caller publishes cursor metadata before retiring the previous manifest.
pub(super) struct Appended {
    pub name: String,
    created: Created,
    retired: Vec<PathBuf>,
}
impl Appended {
    pub fn published(mut self, durable: bool) {
        self.created.published = true;
        if durable {
            for path in &self.retired {
                let _ = fs::remove_file(path);
            }
        }
    }
}

pub(super) fn append(root: &Path, snapshot: &Snapshot, text: &str) -> Result<Appended> {
    let path = snapshot
        .storage_path(root)
        .context("No daily storage manifest")?;
    let mut manifest = Manifest::load(&path)?;
    let mut previous = manifest.parts.last().map(|part| part.day.clone());
    let mut groups: Vec<(String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        let (time, starts, _) = presentation::line_index_info(
            &snapshot.file,
            line.trim_end_matches(['\r', '\n']),
            FixedOffset::east_opt(0).unwrap(),
        );
        let day = if !starts {
            previous
                .clone()
                .or_else(|| time.map(|t| t.date().to_string()))
        } else {
            time.map(|t| t.date().to_string())
        }
        .unwrap_or_else(|| "Unknown".into());
        previous = Some(day.clone());
        if let Some((_, text)) = groups.last_mut().filter(|(date, _)| *date == day) {
            text.push_str(line);
        } else {
            groups.push((day, line.to_owned()));
        }
    }
    let mut created = Created {
        paths: Vec::new(),
        published: false,
    };
    let retired = vec![path.clone()];
    for (day, text) in groups {
        let part = if manifest
            .parts
            .last()
            .is_some_and(|part| !part.frozen && part.day == day)
        {
            let mut previous = manifest.parts.pop().unwrap();
            let previous_path = local_path(path.parent().unwrap(), &previous.path)?;
            let mut output = OpenOptions::new().write(true).open(&previous_path)?;
            if output.metadata()?.len() < previous.bytes {
                bail!("Thawed append source is incomplete");
            }
            // The prior manifest still exposes only its committed prefix if
            // publication is interrupted. Appends stay proportional to new data.
            output.set_len(previous.bytes)?;
            output.seek(SeekFrom::Start(previous.bytes))?;
            output.write_all(text.as_bytes())?;
            output.sync_all()?;
            previous.bytes += text.len() as u64;
            previous
        } else {
            write_part(
                path.parent().unwrap(),
                day,
                text.len() as u64,
                false,
                &mut io::Cursor::new(text.as_bytes()),
                &mut created,
            )?
        };
        manifest.parts.push(part);
    }
    let name = format!("{}.parts", nonce());
    let output = path.parent().unwrap().join(&name);
    created.paths.push(output.clone());
    if !save(&output, &serde_json::to_vec(&manifest)?)? {
        bail!("Could not make appended storage durable");
    }
    Ok(Appended {
        name,
        created,
        retired,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{LogFile, LogViewer, TimeCursor};

    fn fixture() -> (PathBuf, LogViewer) {
        let parent = std::env::temp_dir().join(format!("marklogic-storage-{}", nonce()));
        let root = parent.join("environment");
        fs::create_dir_all(&root).unwrap();
        (
            parent,
            LogViewer {
                cache_root: root,
                ..LogViewer::default()
            },
        )
    }
    fn source(name: &str, host: &str) -> LogFile {
        LogFile {
            name: name.into(),
            host: host.into(),
            path: format!("/Logs/{name}"),
            size: 1000,
            modified: "2026-10-29T10:00:00Z".into(),
            present: false,
        }
    }
    fn committed(logs: &mut LogViewer, file: &LogFile, text: &str) {
        logs.files.push(file.clone());
        logs.commit(file.clone(), true, text, |meta| {
            meta.end = 900;
            meta.length = 1000;
            meta.full_size = Some(text.len() as u64);
            meta.time = Some(TimeCursor {
                ts: "2026-10-29T00:01:00".into(),
                count: 3,
                since: "2026-10-27T00:00:00".into(),
            });
        })
        .unwrap();
    }
    fn saved(logs: &LogViewer, file: &LogFile) -> Snapshot {
        serde_json::from_slice(
            &fs::read(
                logs.cache_root
                    .join(format!("{}.json", hash_key(&file.key()))),
            )
            .unwrap(),
        )
        .unwrap()
    }
    fn contents(logs: &LogViewer, file: &LogFile) -> String {
        let snapshot = saved(logs, file);
        let mut output = String::new();
        snapshot
            .open_content(&logs.cache_root)
            .unwrap()
            .read_to_string(&mut output)
            .unwrap();
        output
    }
    fn select(port: &str, day: &str) -> BTreeSet<CellKey> {
        BTreeSet::from([(port.into(), day.into())])
    }
    fn act(parent: &Path, selected: &BTreeSet<CellKey>, action: Action) -> usize {
        let inventory = scan(parent, |_| {});
        assert!(inventory.warnings.is_empty(), "{:?}", inventory.warnings);
        let (changed, warnings) = apply(&inventory, selected, action, |_| {});
        assert!(warnings.is_empty(), "{warnings:?}");
        changed
    }

    #[test]
    fn scan_reports_overall_sources_bytes_cached_work_and_failures() {
        let (parent, mut logs) = fixture();
        let first = source("8000_ErrorLog.txt", "one");
        let second = source("9000_ErrorLog.txt", "two");
        let first_text = "2026-10-27 12:00:00.000 Error: first\n";
        let second_text = "2026-10-28 12:00:00.000 Error: second\n";
        committed(&mut logs, &first, first_text);
        committed(&mut logs, &second, second_text);
        let total = (first_text.len() + second_text.len()) as u64;
        let mut updates = Vec::new();
        let inventory = scan(&parent, |state| updates.push(state));
        assert!(inventory.warnings.is_empty());
        assert_eq!(updates.first().unwrap().total_sources, 2);
        assert_eq!(updates.first().unwrap().total_bytes, total);
        assert_eq!(updates.first().unwrap().accounted_bytes, 0);
        let final_update = updates.last().unwrap();
        assert_eq!(
            (final_update.completed_sources, final_update.total_sources),
            (2, 2)
        );
        assert_eq!(
            (final_update.accounted_bytes, final_update.total_bytes),
            (total, total)
        );
        assert!(updates.iter().any(|state| {
            state.current.as_ref().is_some_and(|file| {
                file.host == "one" && file.total_bytes == first_text.len() as u64
            })
        }));
        assert!(
            updates
                .windows(2)
                .all(|pair| pair[0].accounted_bytes <= pair[1].accounted_bytes)
        );

        let scan_path = logs
            .cache_root
            .join("storage")
            .join(format!("{}.scan.json", hash_key(&first.key())));
        let written = fs::metadata(&scan_path).unwrap().modified().unwrap();
        let mut cached_updates = Vec::new();
        scan(&parent, |state| cached_updates.push(state));
        assert_eq!(
            fs::metadata(scan_path).unwrap().modified().unwrap(),
            written
        );
        assert_eq!(cached_updates.last().unwrap().accounted_bytes, total);

        // With no saved index, a missing source is accounted for and reported
        // as failed rather than preventing the overall progress from finishing.
        let failed_cache = logs
            .cache_root
            .join("storage")
            .join(format!("{}.scan.json", hash_key(&second.key())));
        fs::remove_file(failed_cache).unwrap();
        fs::remove_file(saved(&logs, &second).content_path(&logs.cache_root)).unwrap();
        let mut failed_updates = Vec::new();
        let inventory = scan(&parent, |state| failed_updates.push(state));
        assert_eq!(inventory.warnings.len(), 1);
        let final_update = failed_updates.last().unwrap();
        assert_eq!(final_update.failed_sources, 1);
        assert_eq!(final_update.completed_sources, 2);
        assert_eq!(final_update.accounted_bytes, total);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn freeze_thaw_and_delete_one_day_preserve_other_days_ports_and_cursors() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        let other = source("9000_ErrorLog.txt", "two");
        let day1 = "2026-10-27 12:00:00.000 Error: first\n  stack continuation\n".repeat(100);
        let day2 = "2026-10-28 12:00:00.000 Info: second\n";
        let raw = format!("{day1}{day2}{day1}");
        committed(&mut logs, &file, &raw);
        committed(&mut logs, &other, day2);
        let selected = select("8000", "2026-10-27");
        assert_eq!(act(&parent, &selected, Action::Archive), 1);
        assert_eq!(contents(&logs, &file), raw);
        assert_eq!(contents(&logs, &other), day2);
        let inventory = scan(&parent, |_| {});
        let cells = inventory.cells();
        assert_eq!(cells[&("8000".into(), "2026-10-27".into())].thawed, 0);
        assert!(cells[&("8000".into(), "2026-10-27".into())].frozen > 0);
        assert!(cells[&("8000".into(), "2026-10-27".into())].frozen < day1.len() as u64);
        assert_eq!(
            cells[&("8000".into(), "2026-10-28".into())].thawed,
            day2.len() as u64
        );
        assert!(!saved(&logs, &file).content_path(&logs.cache_root).exists());
        assert_eq!(
            act(&parent, &selected, Action::Archive),
            0,
            "archive is idempotent"
        );
        assert_eq!(act(&parent, &selected, Action::Thaw), 1);
        assert_eq!(contents(&logs, &file), raw);
        assert_eq!(
            scan(&parent, |_| {}).cells()[&("8000".into(), "2026-10-27".into())].frozen,
            0
        );
        assert_eq!(act(&parent, &selected, Action::Delete), 1);
        assert_eq!(contents(&logs, &file), day2);
        assert_eq!(contents(&logs, &other), day2);
        let snapshot = saved(&logs, &file);
        assert_eq!(snapshot.end, 900, "deletion preserves the remote cursor");
        assert_eq!(snapshot.time.unwrap().count, 3);
        assert!(snapshot.full_size.is_none());
        assert!(
            !scan(&parent, |_| {})
                .cells()
                .contains_key(&("8000".into(), "2026-10-27".into()))
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn frozen_days_survive_restart_reports_export_and_new_live_appends() {
        let (parent, mut logs) = fixture();
        let file = source("ErrorLog.txt", "one");
        let first = "2026-10-27 12:00:00.000 Error: older\n";
        let next = "2026-10-28 12:00:00.000 Info: current\n";
        committed(&mut logs, &file, &format!("{first}{next}"));
        act(&parent, &select("----", "2026-10-27"), Action::Archive);
        logs.finish_storage_operation();
        assert_eq!(logs.snapshots[&file.key()].text, format!("{first}{next}"));
        logs.commit(
            file.clone(),
            false,
            "2026-10-29 08:00:00.000 Info: new\n",
            |meta| meta.end = 950,
        )
        .unwrap();
        let cells = scan(&parent, |_| {}).cells();
        assert!(cells[&("----".into(), "2026-10-27".into())].frozen > 0);
        assert!(cells[&("----".into(), "2026-10-29".into())].thawed > 0);
        assert_eq!(
            contents(&logs, &file),
            format!("{first}{next}2026-10-29 08:00:00.000 Info: new\n")
        );
        let mut visited = Vec::new();
        let mut warnings = Vec::new();
        assert_eq!(
            super::super::visit_cached_error_logs(
                &logs.cache_root,
                |_, line| visited.push(line.to_string()),
                &mut warnings
            )
            .unwrap(),
            1
        );
        assert_eq!(visited.len(), 3);
        assert!(warnings.is_empty());
        logs.finish_storage_operation();
        assert!(logs.snapshots[&file.key()].text.contains("older"));
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn column_and_row_batches_leave_other_cells_untouched() {
        let (parent, mut logs) = fixture();
        let a = source("8000_ErrorLog.txt", "one");
        let b = source("9000_ErrorLog.txt", "two");
        let first = "2026-10-27 12:00:00.000 Error: first\n";
        let next = "2026-10-28 12:00:00.000 Error: next\n";
        committed(&mut logs, &a, &format!("{first}{next}"));
        committed(&mut logs, &b, &format!("{first}{next}"));
        let column = BTreeSet::from([
            ("8000".into(), "2026-10-27".into()),
            ("9000".into(), "2026-10-27".into()),
        ]);
        assert_eq!(act(&parent, &column, Action::Archive), 2);
        let row = BTreeSet::from([
            ("8000".into(), "2026-10-27".into()),
            ("8000".into(), "2026-10-28".into()),
        ]);
        assert_eq!(act(&parent, &row, Action::Delete), 1);
        assert_eq!(contents(&logs, &a), "");
        assert_eq!(contents(&logs, &b), format!("{first}{next}"));
        logs.finish_storage_operation();
        assert_eq!(logs.snapshots[&a.key()].end, 900);
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn parts_reader_seeks_across_frozen_and_thawed_boundaries() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        let a = "2026-10-27 12:00:00.000 Error: aaa\n";
        let b = "2026-10-28 12:00:00.000 Error: bbb\n";
        let raw = format!("{a}{b}");
        committed(&mut logs, &file, &raw);
        act(&parent, &select("8000", "2026-10-27"), Action::Archive);
        let snapshot = saved(&logs, &file);
        let mut reader = snapshot.open_content(&logs.cache_root).unwrap();
        reader.seek(SeekFrom::Start(a.len() as u64 - 2)).unwrap();
        let mut output = [0; 6];
        reader.read_exact(&mut output).unwrap();
        assert_eq!(&output, &raw.as_bytes()[a.len() - 2..a.len() + 4]);
        reader.seek(SeekFrom::End(-4)).unwrap();
        let mut tail = String::new();
        reader.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "bbb\n");
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn live_appends_extend_the_thawed_tail_and_ignore_uncommitted_suffixes() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        let old = "2026-10-27 12:00:00.000 Error: first\n";
        let current = "2026-10-28 12:00:00.000 Error: second\n";
        committed(&mut logs, &file, &format!("{old}{current}"));
        act(&parent, &select("8000", "2026-10-27"), Action::Archive);
        logs.finish_storage_operation();
        let snapshot = saved(&logs, &file);
        let manifest = Manifest::load(&snapshot.storage_path(&logs.cache_root).unwrap()).unwrap();
        let raw_path = logs
            .cache_root
            .join("storage")
            .join(&manifest.parts[1].path);
        use std::io::Write;
        OpenOptions::new()
            .append(true)
            .open(&raw_path)
            .unwrap()
            .write_all(b"uncommitted suffix")
            .unwrap();
        assert_eq!(contents(&logs, &file), format!("{old}{current}"));
        assert_eq!(
            scan(&parent, |_| {}).cells()[&("8000".into(), "2026-10-28".into())].thawed,
            current.len() as u64
        );
        let appended = "2026-10-28 12:01:00.000 Error: appended\n";
        logs.commit(file.clone(), false, appended, |_| {}).unwrap();
        let next =
            Manifest::load(&saved(&logs, &file).storage_path(&logs.cache_root).unwrap()).unwrap();
        assert_eq!(
            manifest.parts[1].path, next.parts[1].path,
            "live polls reuse the thawed tail"
        );
        assert_eq!(next.parts.len(), 2);
        assert_eq!(contents(&logs, &file), format!("{old}{current}{appended}"));
        assert_eq!(
            fs::metadata(raw_path).unwrap().len(),
            (current.len() + appended.len()) as u64
        );
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn legacy_whole_file_archive_can_thaw_one_day_and_keep_the_other_frozen() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        let first = "2026-10-27 12:00:00.000 Error: first\n".repeat(100);
        let second = "2026-10-28 12:00:00.000 Error: second\n".repeat(100);
        let raw = format!("{first}{second}");
        committed(&mut logs, &file, &raw);
        let raw_path = saved(&logs, &file).content_path(&logs.cache_root);
        let archives = logs.cache_root.join("archives");
        fs::create_dir_all(&archives).unwrap();
        archive::build(
            &raw_path,
            &archives.join("legacy.gz"),
            raw.len() as u64,
            &AtomicBool::new(false),
        )
        .unwrap();
        logs.publish_archive(&file.key(), 0, raw.len() as u64, "legacy.gz".into())
            .unwrap();
        fs::remove_file(raw_path).unwrap();
        let before = scan(&parent, |_| {}).cells();
        assert!(before[&("8000".into(), "2026-10-27".into())].estimated);
        assert_eq!(act(&parent, &select("8000", "2026-10-28"), Action::Thaw), 1);
        let cells = scan(&parent, |_| {}).cells();
        assert_eq!(
            cells[&("8000".into(), "2026-10-28".into())].thawed,
            second.len() as u64
        );
        assert!(cells[&("8000".into(), "2026-10-27".into())].frozen > 0);
        assert!(!cells[&("8000".into(), "2026-10-27".into())].estimated);
        assert_eq!(contents(&logs, &file), raw);
        assert!(!archives.join("legacy.gz").exists());
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn record_index_reads_daily_parts_and_rebuilds_after_selected_day_deletion() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        let first = "2026-10-27 12:00:00.000 Error: first\n";
        let second = "2026-10-28 12:00:00.000 Error: second\n";
        committed(&mut logs, &file, &format!("{first}{second}"));
        act(&parent, &select("8000", "2026-10-27"), Action::Archive);
        let index_path = logs.cache_root.join("test.records.sqlite");
        let read = |snapshot: &Snapshot| {
            let spec = crate::logs::ParseSpec {
                generation: snapshot.generation,
                bytes: snapshot.text_bytes,
                offset: FixedOffset::east_opt(0).unwrap(),
                window: crate::logs::TimeWindow::default(),
                before: None,
                paged: false,
                filter_id: 0,
            };
            index::read(
                &snapshot.read_path(&logs.cache_root),
                &index_path,
                &file,
                spec,
                &AtomicU64::new(0),
                &AtomicBool::new(false),
                None,
            )
            .unwrap()
            .0
        };
        assert_eq!(read(&saved(&logs, &file)).records.len(), 2);
        act(&parent, &select("8000", "2026-10-27"), Action::Delete);
        let stream = read(&saved(&logs, &file));
        assert_eq!(stream.records.len(), 1);
        assert!(stream.records[0].raw.contains("second"));
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn full_replacement_retires_the_superseded_daily_layout() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        committed(&mut logs, &file, "2026-10-27 12:00:00.000 Error: first\n");
        act(&parent, &select("8000", "2026-10-27"), Action::Archive);
        logs.finish_storage_operation();
        let old = saved(&logs, &file);
        let old_path = old.storage_path(&logs.cache_root).unwrap();
        let manifest = Manifest::load(&old_path).unwrap();
        let fresh = "2026-10-29 12:00:00.000 Error: replacement\n";
        logs.commit(file.clone(), true, fresh, |_| {}).unwrap();
        assert_eq!(contents(&logs, &file), fresh);
        assert!(!old_path.exists());
        for part in manifest.parts {
            assert!(!logs.cache_root.join("storage").join(part.path).exists());
        }
        fs::remove_dir_all(parent).unwrap();
    }

    #[test]
    fn broken_source_aborts_without_rewriting_metadata() {
        let (parent, mut logs) = fixture();
        let file = source("8000_ErrorLog.txt", "one");
        committed(&mut logs, &file, "2026-10-27 12:00:00.000 Error: first\n");
        let inventory = scan(&parent, |_| {});
        let metadata = inventory.sources[0].metadata.clone();
        let before = fs::read(&metadata).unwrap();
        fs::write(saved(&logs, &file).content_path(&logs.cache_root), b"short").unwrap();
        let (changed, warnings) = apply(
            &inventory,
            &select("8000", "2026-10-27"),
            Action::Archive,
            |_| {},
        );
        assert_eq!(changed, 0);
        assert!(!warnings.is_empty());
        assert_eq!(before, fs::read(metadata).unwrap());
        drop(super::super::ArchivePublishLock::acquire(&logs.cache_root).unwrap());
        fs::remove_dir_all(parent).unwrap();
    }
}
