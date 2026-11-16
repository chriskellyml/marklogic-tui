//! Verified daily compression and mounted-disk archival. Cursor metadata remains authoritative.
use super::{
    archive, hash,
    parts::{Manifest, Part, PartsReader},
    presentation,
    store::{self, Snapshot},
};
use anyhow::{Context, Result, bail};
use chrono::{Days, FixedOffset, NaiveDate};
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub folder: String,
    pub compress_after_days: u32,
    pub archive_after_days: u32,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            folder: String::new(),
            compress_after_days: 7,
            archive_after_days: 7,
        }
    }
}
impl Settings {
    pub fn validate(&self) -> Result<()> {
        if self.compress_after_days == 0
            || self.archive_after_days < self.compress_after_days
            || self.archive_after_days > 36500
        {
            bail!("Use 1–36500 days; archive age must be at least the compression age");
        }
        if !self.folder.is_empty() && !Path::new(&self.folder).is_absolute() {
            bail!("Choose an absolute path to a mounted archive folder");
        }
        Ok(())
    }
}
pub fn load(root: &Path) -> Result<Settings> {
    let path = root.join("archive-settings.cfg");
    if !path.exists() {
        return Ok(Settings::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn save(root: &Path, settings: &Settings) -> Result<()> {
    settings.validate()?;
    store::publish(
        &root.join("archive-settings.cfg"),
        &serde_json::to_vec(settings)?,
    )
}
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    pub compressed: usize,
    pub archived: usize,
    pub retired_bytes: u64,
    pub index_bytes_reclaimed: u64,
    pub warnings: Vec<String>,
}
fn check(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        bail!("Archive cancelled; completed sources are preserved");
    }
    Ok(())
}
fn unique() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}
pub(super) fn lease(root: &Path, name: &str, shared: bool) -> Result<File> {
    fs::create_dir_all(root)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(name))?;
    if shared { file.try_lock_shared() } else { file.try_lock() }.context("Another log reader or storage operation is using this cache; close other log viewers and retry")?;
    Ok(file)
}
fn durable_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    Ok(())
}

#[derive(Default)]
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

struct Range {
    day: String,
    offset: u64,
    bytes: u64,
    records: Option<u64>,
}
fn ranges(
    snapshot: &Snapshot,
    root: &Path,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(&str, u64),
) -> Result<Vec<Range>> {
    ranges_from(snapshot, root, cancel, progress, None, false)
}
fn ranges_from(
    snapshot: &Snapshot,
    root: &Path,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(&str, u64),
    previous: Option<String>,
    has_record: bool,
) -> Result<Vec<Range>> {
    let mut input = BufReader::new(snapshot.open(root)?.take(snapshot.bytes()));
    let mut line = Vec::new();
    let mut ranges: Vec<Range> = Vec::new();
    let mut offset = 0;
    let mut previous = previous;
    let mut has_record = has_record;
    let zone = chrono::DateTime::parse_from_rfc3339(&snapshot.file.modified)
        .map(|t| *t.offset())
        .unwrap_or(FixedOffset::east_opt(0).unwrap());
    loop {
        check(cancel)?;
        line.clear();
        let count = input
            .by_ref()
            .take(4 * 1024 * 1024 + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count > 4 * 1024 * 1024 {
            bail!("Log line exceeds the 4 MiB storage reader limit");
        }
        let raw = std::str::from_utf8(&line)?.trim_end_matches(['\r', '\n']);
        let (time, starts, _) = presentation::line_index_info(&snapshot.file, raw, zone);
        let day = if !starts || raw.trim().is_empty() {
            previous
                .clone()
                .or_else(|| time.map(|t| t.date().to_string()))
        } else {
            time.map(|t| t.date().to_string())
        };
        previous = day.clone();
        let day = day.unwrap_or_else(|| "Unknown".into());
        if let Some(last) = ranges.last_mut().filter(|r| r.day == day) {
            last.bytes += count as u64;
        } else {
            ranges.push(Range {
                day,
                offset,
                bytes: count as u64,
                records: Some(0),
            });
        }
        if !raw.trim().is_empty() && !raw.starts_with("==>") {
            if starts || !has_record {
                *ranges.last_mut().unwrap().records.as_mut().unwrap() += 1;
            }
            has_record = true;
        }
        offset += count as u64;
        if offset / (1024 * 1024) != (offset - count as u64) / (1024 * 1024) {
            progress(&snapshot.file.name, offset);
        }
    }
    if offset != snapshot.bytes() {
        bail!("Incomplete committed log content");
    }
    Ok(ranges)
}
/// Append only new bytes, preserving compressed and offloaded earlier parts.
pub(super) fn append_parts(
    root: &Path,
    snapshot: &Snapshot,
    text: &str,
    cancel: &AtomicBool,
) -> Result<String> {
    let storage = root.join("storage");
    let old_name = snapshot
        .storage
        .as_deref()
        .context("Missing daily storage")?;
    let mut manifest = Manifest::load(&super::parts::local_path(&storage, old_name)?)?;
    let mut tail = snapshot.clone();
    tail.text = text.to_owned();
    tail.text_bytes = text.len() as u64;
    let previous = manifest.parts.last().map(|p| p.day.clone());
    let ranges = if text.is_empty() {
        Vec::new()
    } else {
        ranges_from(
            &tail,
            root,
            cancel,
            &mut |_, _| {},
            previous,
            snapshot.bytes() > 0,
        )?
    };
    let mut created = Created::default();
    for range in ranges {
        check(cancel)?;
        let start = range.offset as usize;
        let end = start + range.bytes as usize;
        if let Some(part) = manifest
            .parts
            .last_mut()
            .filter(|p| p.day == range.day && !p.frozen && p.remote.is_none())
        {
            let path = super::parts::local_path(&storage, &part.path)?;
            let mut output = OpenOptions::new().write(true).open(path)?;
            if output.metadata()?.len() < part.bytes {
                bail!("Incomplete existing daily partition");
            }
            // A failed earlier append may have an uncommitted suffix. Readers
            // of older manifests are bounded to their committed byte prefix.
            output.set_len(part.bytes)?;
            output.seek(SeekFrom::Start(part.bytes))?;
            output.write_all(&text.as_bytes()[start..end])?;
            output.sync_all()?;
            part.bytes += range.bytes;
            part.records = part
                .records
                .zip(range.records)
                .and_then(|(a, b)| a.checked_add(b));
            continue;
        }
        let name = format!("{}-{}.log", range.day, unique());
        let path = storage.join(&name);
        created.paths.push(path.clone());
        store::publish(&path, &text.as_bytes()[start..end])?;
        manifest.parts.push(Part {
            day: range.day,
            path: name,
            bytes: range.bytes,
            frozen: false,
            remote: None,
            records: range.records,
        });
    }
    let name = format!("{}.parts", unique());
    let path = storage.join(&name);
    created.paths.push(path.clone());
    store::publish(&path, &serde_json::to_vec(&manifest)?)?;
    durable_directory(&storage)?;
    // The caller publishes its cursor next. Retain these small append files on
    // publication failure so an existing cursor can never lose shared parts.
    created.published = true;
    Ok(name)
}

fn old(day: &str, cutoff: NaiveDate) -> bool {
    NaiveDate::parse_from_str(day, "%Y-%m-%d").is_ok_and(|d| d < cutoff)
}

/// The remote root must already exist: never create a missing mount point.
/// Every source is committed independently; errors never retire its originals.
pub fn run(
    root: &Path,
    settings: &Settings,
    today: NaiveDate,
    cancel: &AtomicBool,
    mut progress: impl FnMut(&str, u64),
) -> Result<Report> {
    settings.validate()?;
    if settings.folder.trim().is_empty() {
        bail!("Choose an archive folder first");
    }
    let remote = fs::canonicalize(&settings.folder)
        .context("Archive folder unavailable; mount the archive disk and retry")?;
    if !remote.is_dir() {
        bail!("Archive destination must be an existing folder");
    }
    fs::create_dir_all(root)?;
    let local = fs::canonicalize(root)?;
    if remote.starts_with(&local) || local.starts_with(&remote) {
        bail!("Archive folder must be separate from the log cache");
    }
    let _storage = lease(root, ".storage.lock", false)?;
    let _desktop = lease(&root.join("desktop"), ".storage.lock", false)?;
    let _index = lease(&root.join("desktop"), ".index.lock", false)?;
    let remote = remote
        .join("marklogic-logs")
        .join(local.file_name().context("Missing cache identity")?);
    fs::create_dir_all(remote.join("storage"))?;
    let compress_cutoff = today
        .checked_sub_days(Days::new(settings.compress_after_days as u64))
        .context("Invalid compression age")?;
    let archive_cutoff = today
        .checked_sub_days(Days::new(settings.archive_after_days as u64))
        .context("Invalid archive age")?;
    let mut report = Report::default();
    for directory in [root.to_path_buf(), root.join("desktop")] {
        let mut paths = vec![directory.clone()];
        if directory.join("history").exists() {
            paths.push(directory.join("history"));
        }
        let mut entries = Vec::new();
        for path in paths {
            entries.extend(fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?);
        }
        for entry in entries {
            check(cancel)?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let bytes = fs::read(&path)?;
            let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else {
                continue;
            };
            progress(&snapshot.file.name, 0);
            if let Err(error) = process(
                &directory,
                &path,
                &bytes,
                &snapshot,
                &remote,
                compress_cutoff,
                archive_cutoff,
                cancel,
                &mut progress,
                &mut report,
                None,
            ) {
                check(cancel)?;
                report.warnings.push(format!(
                    "{} / {}: {error:#}",
                    snapshot.file.host, snapshot.file.name
                ));
            }
        }
    }
    let retained = root.join("retained-history.cfg");
    if retained.exists() {
        let document: serde_json::Value = serde_json::from_slice(&fs::read(&retained)?)?;
        let keys: Vec<_> = document["entries"]
            .as_object()
            .map(|entries| entries.keys().cloned().collect())
            .unwrap_or_default();
        for key in keys {
            check(cancel)?;
            let bytes = fs::read(&retained)?;
            let document: serde_json::Value = serde_json::from_slice(&bytes)?;
            let snapshot: Snapshot =
                serde_json::from_value(document["entries"][&key]["snapshot"].clone())?;
            if let Err(error) = process(
                root,
                &retained,
                &bytes,
                &snapshot,
                &remote,
                compress_cutoff,
                archive_cutoff,
                cancel,
                &mut progress,
                &mut report,
                Some(&key),
            ) {
                check(cancel)?;
                report.warnings.push(format!(
                    "Retained {} / {}: {error:#}",
                    snapshot.file.host, snapshot.file.name
                ));
            }
        }
    }
    check(cancel)?;
    progress("Reclaiming the rebuildable record index", 0);
    // All readers hold a shared lease for the lifetime of their SQLite connection.
    // Replace the derived index with an empty database instead of VACUUM, which
    // could require gigabytes of temporary space on an already full disk.
    report.index_bytes_reclaimed = reset_index(root)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn process(
    root: &Path,
    metadata: &Path,
    original: &[u8],
    snapshot: &Snapshot,
    remote: &Path,
    compress_cutoff: NaiveDate,
    archive_cutoff: NaiveDate,
    cancel: &AtomicBool,
    progress: &mut impl FnMut(&str, u64),
    report: &mut Report,
    retained_key: Option<&str>,
) -> Result<()> {
    let storage = root.join("storage");
    let previous = snapshot
        .storage
        .as_ref()
        .map(|name| super::parts::local_path(&storage, name).and_then(|p| Manifest::load(&p)))
        .transpose()?;
    let ranges = if let Some(manifest) = &previous {
        let mut offset = 0;
        manifest
            .parts
            .iter()
            .map(|p| {
                let range = Range {
                    day: p.day.clone(),
                    offset,
                    bytes: p.bytes,
                    records: p.records,
                };
                offset += p.bytes;
                range
            })
            .collect()
    } else {
        ranges(snapshot, root, cancel, progress)?
    };
    if !ranges.iter().enumerate().any(|(i, r)| {
        old(&r.day, compress_cutoff)
            && previous
                .as_ref()
                .and_then(|m| m.parts.get(i))
                .is_none_or(|p| !p.frozen || (p.remote.is_none() && old(&r.day, archive_cutoff)))
    }) {
        return Ok(());
    }
    fs::create_dir_all(&storage)?;
    let mut input = snapshot.open(root)?;
    let mut manifest = Manifest::default();
    // Clean up unpublished output on errors. Originals remain until both
    // the replacement manifest and cursor are durable.
    let mut created = Created::default();
    let mut compressed = 0;
    let mut archived = 0;
    for (i, range) in ranges.iter().enumerate() {
        check(cancel)?;
        let existing = previous.as_ref().and_then(|m| m.parts.get(i));
        let frozen = old(&range.day, compress_cutoff) || existing.is_some_and(|p| p.frozen);
        let offload = old(&range.day, archive_cutoff);
        if let Some(part) =
            existing.filter(|p| p.frozen == frozen && (!offload || p.remote.is_some()))
        {
            manifest.parts.push(part.clone());
            continue;
        }
        input.seek(SeekFrom::Start(range.offset))?;
        let name = format!(
            "{}-{}.{}",
            range.day,
            unique(),
            if frozen { "log.gz" } else { "log" }
        );
        let destination = if offload {
            remote.join("storage").join(&name)
        } else {
            storage.join(&name)
        };
        created.paths.push(destination.clone());
        if frozen {
            created.paths.push(archive::sidecar(&destination));
            archive::build_reader(
                (&mut input).take(range.bytes),
                &destination,
                range.bytes,
                cancel,
                Compression::best(),
            )?;
            archive::verify(&destination, cancel)?;
            compressed += usize::from(!existing.is_some_and(|p| p.frozen));
            archived += usize::from(offload);
        } else {
            let mut output = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&destination)?;
            let copied = std::io::copy(&mut (&mut input).take(range.bytes), &mut output)?;
            if copied != range.bytes {
                bail!("Incomplete daily partition");
            }
            output.sync_all()?;
        }
        manifest.parts.push(Part {
            day: range.day.clone(),
            path: name,
            bytes: range.bytes,
            frozen,
            remote: offload.then_some(destination),
            records: range.records,
        });
        progress(&snapshot.file.name, range.offset + range.bytes);
    }
    let name = format!("{}.parts", unique());
    let path = storage.join(&name);
    created.paths.push(path.clone());
    store::publish(&path, &serde_json::to_vec(&manifest)?)?;
    durable_directory(&storage)?;
    let verified = PartsReader::open(&path)?;
    if verified.len() != snapshot.bytes() {
        bail!("Daily storage length mismatch");
    }
    drop(verified);
    // A portable recovery snapshot accompanies each remote subset. It uses
    // relative paths so ordinary tools can recover it even without this cache.
    let remote_parts: Vec<_> = manifest
        .parts
        .iter()
        .filter(|p| p.remote.is_some())
        .map(|p| Part {
            remote: p
                .remote
                .as_ref()
                .filter(|path| path.parent() != Some(remote.join("storage").as_path()))
                .cloned(),
            ..p.clone()
        })
        .collect();
    if !remote_parts.is_empty() {
        let recovery_name = format!("{}-{}", hash(&snapshot.file.key()), unique());
        let recovery_manifest = Manifest {
            parts: remote_parts,
        };
        let recovery_bytes = recovery_manifest.bytes();
        store::publish(
            &remote
                .join("storage")
                .join(format!("{recovery_name}.parts")),
            &serde_json::to_vec(&recovery_manifest)?,
        )?;
        let mut recovery = serde_json::to_value(snapshot)?;
        recovery["storage"] = serde_json::json!(format!("{recovery_name}.parts"));
        recovery["text_bytes"] = serde_json::json!(recovery_bytes);
        recovery["archive"] = serde_json::Value::Null;
        recovery["full_size"] = serde_json::Value::Null;
        recovery["recovery_format"] = serde_json::json!(1);
        store::publish(
            &remote.join(format!("{recovery_name}.recovery.json")),
            &serde_json::to_vec(&recovery)?,
        )?;
        // Recovery metadata now owns these remote files independently.
        created.paths.retain(|p| !p.starts_with(remote));
        durable_directory(&remote.join("storage"))?;
        durable_directory(remote)?;
    }
    check(cancel)?;
    if fs::read(metadata)? != original {
        bail!("Log changed during archival; originals retained");
    }
    // Preserve unknown fields and cursor values for both interfaces.
    let mut document: serde_json::Value = serde_json::from_slice(original)?;
    let updated = if let Some(key) = retained_key {
        &mut document["entries"][key]["snapshot"]
    } else {
        &mut document
    };
    updated["storage"] = serde_json::json!(name);
    updated["archive"] = serde_json::Value::Null;
    updated["text_bytes"] = serde_json::json!(snapshot.bytes());
    updated.as_object_mut().unwrap().remove("text");
    store::publish(metadata, &serde_json::to_vec(&document)?)?;
    created.published = true;
    durable_directory(metadata.parent().context("Missing metadata directory")?)?;
    report.compressed += compressed;
    report.archived += archived;
    drop(input);
    let keep: std::collections::BTreeSet<_> =
        manifest.parts.iter().map(|p| p.path.as_str()).collect();
    // A retained generation can share unchanged parts with a newer cursor.
    // Only unlink paths that no remaining committed source references.
    let cache_root = if root.file_name().is_some_and(|n| n == "desktop") {
        root.parent().context("Missing cache root")?
    } else {
        root
    };
    let mut referenced = std::collections::BTreeSet::new();
    for source in store::catalog(cache_root)?.sources.values() {
        if let Some(name) = &source.snapshot.storage {
            let path = super::parts::local_path(&source.root.join("storage"), name)?;
            let manifest = Manifest::load(&path)?;
            referenced.insert(path);
            for part in manifest.parts {
                if part.remote.is_none() {
                    referenced.insert(super::parts::local_path(
                        &source.root.join("storage"),
                        &part.path,
                    )?);
                }
            }
        }
    }
    let mut retire = |path: PathBuf| {
        if referenced.contains(&path) {
            return;
        }
        let size = fs::metadata(&path).map_or(0, |m| m.len());
        match fs::remove_file(&path) {
            Ok(()) => report.retired_bytes += size,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report
                .warnings
                .push(format!("Could not retire {}: {e}", path.display())),
        }
    };
    if let Some(previous) = previous {
        for part in previous.parts {
            // Remote archives are permanent, including after log rotation.
            if part.remote.is_none() && !keep.contains(part.path.as_str()) {
                let path = super::parts::local_path(&storage, &part.path)?;
                retire(path.clone());
                if part.frozen {
                    retire(archive::sidecar(&path));
                }
            }
        }
        retire(super::parts::local_path(
            &storage,
            snapshot.storage.as_ref().unwrap(),
        )?);
    } else {
        let id = hash(&snapshot.file.key());
        retire(root.join(if snapshot.generation_path {
            format!("{id}.{}.generation.log", snapshot.generation)
        } else {
            format!("{id}.log")
        }));
        retire(root.join(format!("{id}.{}.previous.log", snapshot.generation)));
        if let Some(name) = &snapshot.archive {
            let path = super::parts::local_path(&root.join("archives"), name)?;
            retire(path.clone());
            retire(archive::sidecar(&path));
        }
    }
    Ok(())
}

fn reset_index(root: &Path) -> Result<u64> {
    let folder = root.join("desktop");
    let mut reclaimed = 0;
    for name in ["records.sqlite", "records.sqlite-wal", "records.sqlite-shm"] {
        let path = folder.join(name);
        if let Ok(metadata) = fs::metadata(&path) {
            fs::remove_file(&path)?;
            reclaimed += metadata.len();
        }
    }
    durable_directory(&folder)?;
    Ok(reclaimed)
}

#[cfg(test)]
mod tests {
    use super::super::{
        LogFile,
        store::{CellScope, Chunk, Query},
    };
    use super::*;
    use std::sync::Arc;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("log-retention-{}", unique()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn source(root: &Path, text: &str) -> String {
        let file = LogFile {
            host: "host".into(),
            path: "/Logs/ErrorLog.txt".into(),
            name: "ErrorLog.txt".into(),
            size: text.len() as u64,
            modified: "2026-11-16T12:00:00+01:00".into(),
            present: true,
        };
        let id = hash(&file.key());
        let chunk = Chunk {
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
        };
        let old = store::catalog(root).unwrap().sources.get(&id).cloned();
        store::save_chunk(root, file, chunk, old.as_ref(), &AtomicBool::new(false)).unwrap();
        id
    }
    fn settings(remote: &Path) -> Settings {
        Settings {
            folder: remote.to_string_lossy().into(),
            archive_after_days: 14,
            ..Settings::default()
        }
    }
    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 16).unwrap()
    }
    fn read(root: &Path, id: &str) -> String {
        let catalog = store::catalog(root).unwrap();
        let source = &catalog.sources[id];
        let mut text = String::new();
        source
            .snapshot
            .open(&source.root)
            .unwrap()
            .take(source.snapshot.bytes())
            .read_to_string(&mut text)
            .unwrap();
        text
    }
    const TEXT: &str = "undated header\r\n2026-11-01 09:00:00.000 Info: archive me\r\n  continuation\r\n2026-11-02 09:00:00.000 Info: boundary stays local\n2026-11-08 09:00:00.000 Info: compress me\n2026-11-09 09:00:00.000 Info: recent boundary\n2026-11-16 09:00:00.000 Info: recent ü\n";
    #[test]
    fn daily_tiers_preserve_bytes_cursors_boundaries_and_reclaim_index() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let before = store::catalog(&local.0).unwrap().sources[&id]
            .snapshot
            .clone();
        store::page(
            &local.0,
            &Query {
                files: vec![id.clone()],
                ..Query::default()
            },
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        let config = settings(&remote.0);
        save(&local.0, &config).unwrap();
        let report = run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.compressed, 3);
        assert_eq!(report.archived, 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert!(report.index_bytes_reclaimed > 0);
        assert!(!local.0.join("desktop/records.sqlite").exists());
        assert_eq!(read(&local.0, &id), TEXT);
        let catalog = store::catalog(&local.0).unwrap();
        let after = &catalog.sources[&id].snapshot;
        assert_eq!(
            (
                after.start,
                after.end,
                after.length,
                after.generation,
                after.full_size
            ),
            (
                before.start,
                before.end,
                before.length,
                before.generation,
                before.full_size
            )
        );
        let manifest = Manifest::load(
            &local
                .0
                .join("desktop/storage")
                .join(after.storage.as_ref().unwrap()),
        )
        .unwrap();
        assert_eq!(
            manifest
                .parts
                .iter()
                .map(|p| (p.day.as_str(), p.frozen, p.remote.is_some()))
                .collect::<Vec<_>>(),
            vec![
                ("Unknown", false, false),
                ("2026-11-01", true, true),
                ("2026-11-02", true, false),
                ("2026-11-08", true, false),
                ("2026-11-09", false, false),
                ("2026-11-16", false, false)
            ]
        );
        assert!(
            !local
                .0
                .join(format!("desktop/{id}.{}.generation.log", before.generation))
                .exists()
        );
        let again = run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(
            (again.compressed, again.archived, again.retired_bytes),
            (0, 0, 0)
        );
        let page = store::page(
            &local.0,
            &Query {
                files: vec![id],
                ..Query::default()
            },
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert!(page.rows.iter().any(|r| r.raw.contains("archive me")));
    }
    #[test]
    fn recovery_snapshots_survive_rotation_and_loss_of_local_metadata() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let config = settings(&remote.0);
        save(&local.0, &config).unwrap();
        run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        source(&local.0, "2026-11-16 10:00:00.000 Info: replacement\n");
        let catalog = store::catalog(&local.0).unwrap();
        assert_eq!(catalog.sources.len(), 2);
        let archived = catalog
            .sources
            .iter()
            .find(|(key, _)| *key != &id)
            .unwrap()
            .0
            .clone();
        assert_eq!(read(&local.0, &archived), TEXT);
        fs::remove_file(local.0.join(format!("desktop/{id}.json"))).unwrap();
        fs::remove_dir_all(local.0.join("desktop/history")).unwrap();
        assert_eq!(store::catalog(&local.0).unwrap().sources.len(), 1);
        let recovered = store::page(
            &local.0,
            &Query {
                files: vec![archived],
                ..Query::default()
            },
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(recovered.total, 1);
        assert!(recovered.rows[0].raw.contains("archive me"));
    }
    #[test]
    fn missing_disk_cancellation_and_live_readers_preserve_originals() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let metadata = local.0.join(format!("desktop/{id}.json"));
        let original = fs::read(&metadata).unwrap();
        let missing = settings(&remote.0.join("missing-mount"));
        assert!(
            run(
                &local.0,
                &missing,
                today(),
                &AtomicBool::new(false),
                |_, _| {}
            )
            .is_err()
        );
        assert!(!remote.0.join("missing-mount").exists());
        let config = settings(&remote.0);
        assert!(
            run(
                &local.0,
                &config,
                today(),
                &AtomicBool::new(true),
                |_, _| {}
            )
            .is_err()
        );
        let reader = store::Reader::open(&local.0).unwrap();
        assert!(
            run(
                &local.0,
                &config,
                today(),
                &AtomicBool::new(false),
                |_, _| {}
            )
            .is_err()
        );
        drop(reader);
        assert_eq!(fs::read(metadata).unwrap(), original);
        assert_eq!(read(&local.0, &id), TEXT);
    }
    #[test]
    fn recent_cells_read_without_archive_disk_and_keep_source_ordinals() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let config = settings(&remote.0);
        save(&local.0, &config).unwrap();
        run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        let detached = remote.0.with_extension("detached");
        fs::rename(&remote.0, &detached).unwrap();
        let query = Query {
            files: vec![id.clone()],
            cells: vec![CellScope {
                source: id.clone(),
                day: "2026-11-16".into(),
            }],
            ..Query::default()
        };
        let recent = store::page(
            &local.0,
            &query,
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(recent.total, 1);
        assert_eq!(recent.rows[0].ordinal, 6);
        assert!(recent.rows[0].raw.contains("recent ü"));
        assert!(recent.coverage[&id].contains(&"2026-11-01".into()));
        let archived = Query {
            cells: vec![CellScope {
                source: id,
                day: "2026-11-01".into(),
            }],
            ..query
        };
        assert!(
            store::page(
                &local.0,
                &archived,
                Arc::new(AtomicBool::new(false)),
                |_, _| {}
            )
            .is_err()
        );
        fs::rename(detached, &remote.0).unwrap();
        let restored = store::page(
            &local.0,
            &archived,
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(restored.total, 1);
        assert_eq!(restored.rows[0].ordinal, 2);
    }
    #[test]
    fn cancellation_after_a_segment_preserves_original_and_removes_local_staging() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let original = fs::read(local.0.join(format!("desktop/{id}.json"))).unwrap();
        let cancel = AtomicBool::new(false);
        assert!(
            run(
                &local.0,
                &settings(&remote.0),
                today(),
                &cancel,
                |_, bytes| {
                    if bytes > 0 {
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            )
            .is_err()
        );
        assert_eq!(
            fs::read(local.0.join(format!("desktop/{id}.json"))).unwrap(),
            original
        );
        assert_eq!(read(&local.0, &id), TEXT);
        assert_eq!(
            fs::read_dir(local.0.join("desktop/storage"))
                .unwrap()
                .count(),
            0
        );
    }
    #[test]
    fn tail_append_keeps_archived_parts_and_reads_recent_continuations_offline() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let config = settings(&remote.0);
        save(&local.0, &config).unwrap();
        run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        let old = store::catalog(&local.0).unwrap().sources[&id].clone();
        let old_manifest = Manifest::load(
            &local
                .0
                .join("desktop/storage")
                .join(old.snapshot.storage.as_ref().unwrap()),
        )
        .unwrap();
        let detached = remote.0.with_extension("detached");
        fs::rename(&remote.0, &detached).unwrap();
        let tail = "  tail continuation\n2026-11-16 10:00:00.000 Info: appended\n2026-11-17 10:00:00.000 Info: next day\n";
        let mut file = old.snapshot.file.clone();
        file.size += tail.len() as u64;
        let end = old.snapshot.end + tail.chars().count() as u64;
        store::save_chunk(
            &local.0,
            file.clone(),
            Chunk {
                host: file.host,
                path: file.path,
                start: old.snapshot.end,
                end,
                length: end,
                reset: false,
                full: false,
                text: tail.into(),
                anchor_start: 0,
                anchor: String::new(),
                missing: false,
                error: None,
            },
            Some(&old),
            &AtomicBool::new(false),
        )
        .unwrap();
        let catalog = store::catalog(&local.0).unwrap();
        let source = &catalog.sources[&id];
        let manifest = Manifest::load(
            &local
                .0
                .join("desktop/storage")
                .join(source.snapshot.storage.as_ref().unwrap()),
        )
        .unwrap();
        assert_eq!(manifest.parts[1].remote, old_manifest.parts[1].remote);
        assert_eq!(manifest.parts.len(), old_manifest.parts.len() + 1);
        assert_eq!(manifest.bytes(), (TEXT.len() + tail.len()) as u64);
        let query = Query {
            files: vec![id.clone()],
            cells: vec![CellScope {
                source: id.clone(),
                day: "2026-11-16".into(),
            }],
            ..Query::default()
        };
        let page = store::page(
            &local.0,
            &query,
            Arc::new(AtomicBool::new(false)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.rows[0].ordinal, 6);
        assert!(page.rows[0].raw.contains("tail continuation"));
        assert_eq!(page.rows[1].ordinal, 7);
        fs::rename(detached, &remote.0).unwrap();
        assert_eq!(read(&local.0, &id), format!("{TEXT}{tail}"));
    }
    #[test]
    fn retained_tui_catalog_is_archived_without_losing_other_entry_fields() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let metadata = local.0.join(format!("desktop/{id}.json"));
        let snapshot: serde_json::Value =
            serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
        let raw = local.0.join(format!(
            "desktop/{id}.{}.generation.log",
            snapshot["generation"].as_u64().unwrap()
        ));
        fs::rename(&raw, local.0.join(raw.file_name().unwrap())).unwrap();
        fs::remove_file(metadata).unwrap();
        let retained = serde_json::json!({"offset":3600,"entries":{"retained": {"snapshot":snapshot,"days":["2026-11-01"],"extra":"preserve me"}}});
        fs::write(
            local.0.join("retained-history.cfg"),
            serde_json::to_vec(&retained).unwrap(),
        )
        .unwrap();
        let config = settings(&remote.0);
        save(&local.0, &config).unwrap();
        let report = run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.archived, 1);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(read(&local.0, &id), TEXT);
        let after: serde_json::Value =
            serde_json::from_slice(&fs::read(local.0.join("retained-history.cfg")).unwrap())
                .unwrap();
        assert_eq!(after["entries"]["retained"]["extra"], "preserve me");
        assert_eq!(after["offset"], 3600);
    }
    #[test]
    fn direct_archive_and_settings_validation() {
        let local = Temp::new();
        let remote = Temp::new();
        let id = source(&local.0, TEXT);
        let mut config = settings(&remote.0);
        config.archive_after_days = 7;
        save(&local.0, &config).unwrap();
        assert_eq!(load(&local.0).unwrap().archive_after_days, 7);
        let report = run(
            &local.0,
            &config,
            today(),
            &AtomicBool::new(false),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(report.archived, 3);
        assert_eq!(read(&local.0, &id), TEXT);
        config.archive_after_days = 6;
        assert!(save(&local.0, &config).is_err());
        config.archive_after_days = 7;
        config.folder = "relative/path".into();
        assert!(save(&local.0, &config).is_err());
    }
}
