use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const MANIFEST_VERSION: u32 = 1;
const ORIGINALS_DIR_NAME: &str = ".originals";
const MANIFEST_FILE_NAME: &str = ".manifest.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedModuleEntry {
    pub rel: String,
    pub original_hash: String,
    pub synced_hash: String,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CachedModuleManifest {
    #[serde(default = "default_manifest_version")]
    pub version: u32,
    #[serde(default)]
    pub entries: HashMap<String, CachedModuleEntry>,
}

fn default_manifest_version() -> u32 {
    MANIFEST_VERSION
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedModuleStatus {
    pub uri: String,
    pub rel: String,
    pub working_path: PathBuf,
    pub dirty: bool,
    pub unsynced: bool,
    pub missing_original: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Del,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// FNV-1a 64-bit hash, hex-encoded. Deterministic across runs without new deps.
pub fn content_hash(content: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{:016x}", hash)
}

fn sanitize_db_name(db: &str) -> String {
    let trimmed = db.trim();
    if trimmed.is_empty() {
        return "default".to_string();
    }
    trimmed
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Map a MarkLogic module URI to a relative filesystem path.
/// Strips the leading `/`, rejects `..` escapes (replaced with `_`).
pub fn uri_to_rel_path(uri: &str) -> PathBuf {
    let stripped = uri.trim().trim_start_matches('/');
    let mut rel = PathBuf::new();
    for part in stripped.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            rel.push("__parent__");
            continue;
        }
        // Guard against windows-hostile characters but keep readability.
        let clean: String = part
            .chars()
            .map(|c| match c {
                '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
                _ => c,
            })
            .collect();
        // Path::push would treat absolute components specially; split again.
        let piece = PathBuf::from(clean);
        // Flatten any embedded separators introduced above (shouldn't happen).
        let mut flat = PathBuf::new();
        for comp in piece.components() {
            if let Component::Normal(os) = comp {
                flat.push(os);
            }
        }
        if flat.as_os_str().is_empty() {
            continue;
        }
        rel.push(flat);
    }
    if rel.as_os_str().is_empty() {
        rel.push("unnamed-module");
    }
    // Avoid a trailing-slash URI mapping to a directory.
    if uri.trim_end().ends_with('/') {
        rel.push("index");
    }
    rel
}

pub fn modules_db_root(cache_dir: &Path, db: &str) -> PathBuf {
    cache_dir.join("modules").join(sanitize_db_name(db))
}

pub fn working_path_for(cache_dir: &Path, db: &str, uri: &str) -> PathBuf {
    modules_db_root(cache_dir, db).join(uri_to_rel_path(uri))
}

pub fn original_path_for(cache_dir: &Path, db: &str, uri: &str) -> PathBuf {
    modules_db_root(cache_dir, db)
        .join(ORIGINALS_DIR_NAME)
        .join(uri_to_rel_path(uri))
}

pub fn manifest_path_for(cache_dir: &Path, db: &str) -> PathBuf {
    modules_db_root(cache_dir, db).join(MANIFEST_FILE_NAME)
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn load_manifest(cache_dir: &Path, db: &str) -> Result<CachedModuleManifest> {
    let path = manifest_path_for(cache_dir, db);
    if !path.exists() {
        return Ok(CachedModuleManifest {
            version: MANIFEST_VERSION,
            entries: HashMap::new(),
        });
    }
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read modules manifest: {}", path.display()))?;
    let manifest: CachedModuleManifest = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse modules manifest: {}", path.display()))?;
    Ok(manifest)
}

pub fn save_manifest(cache_dir: &Path, db: &str, manifest: &CachedModuleManifest) -> Result<()> {
    let root = modules_db_root(cache_dir, db);
    fs::create_dir_all(&root)
        .with_context(|| format!("Failed to create modules cache: {}", root.display()))?;
    let path = manifest_path_for(cache_dir, db);
    let content = serde_json::to_string_pretty(manifest).context("Failed to serialize manifest")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &content)
        .with_context(|| format!("Failed to write manifest: {}", tmp.display()))?;
    fs::rename(&tmp, &path)
        .with_context(|| format!("Failed to finalize manifest: {}", path.display()))?;
    Ok(())
}

/// Write working + original copies and record hashes in the manifest.
/// Returns the working file path.
pub fn checkout_module(
    cache_dir: &Path,
    db: &str,
    uri: &str,
    remote_content: &str,
) -> Result<PathBuf> {
    let working = working_path_for(cache_dir, db, uri);
    let original = original_path_for(cache_dir, db, uri);
    if let Some(parent) = working.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache dir: {}", parent.display()))?;
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create originals dir: {}", parent.display()))?;
    }
    fs::write(&original, remote_content)
        .with_context(|| format!("Failed to write original copy: {}", original.display()))?;
    // Only create the working copy on first checkout; keep local edits otherwise.
    if !working.exists() {
        fs::write(&working, remote_content)
            .with_context(|| format!("Failed to write working copy: {}", working.display()))?;
    }
    let hash = content_hash(remote_content);
    let mut manifest = load_manifest(cache_dir, db)?;
    let rel = uri_to_rel_path(uri).display().to_string();
    manifest.entries.insert(
        uri.to_string(),
        CachedModuleEntry {
            rel,
            original_hash: hash.clone(),
            synced_hash: manifest
                .entries
                .get(uri)
                .map(|e| e.synced_hash.clone())
                .unwrap_or(hash),
            updated_at: now_secs(),
        },
    );
    save_manifest(cache_dir, db, &manifest)?;
    Ok(working)
}

pub fn mark_synced(cache_dir: &Path, db: &str, uri: &str, pushed_content: &str) -> Result<()> {
    let mut manifest = load_manifest(cache_dir, db)?;
    let rel = uri_to_rel_path(uri).display().to_string();
    let hash = content_hash(pushed_content);
    manifest
        .entries
        .entry(uri.to_string())
        .and_modify(|e| {
            e.synced_hash = hash.clone();
            e.rel = rel.clone();
            e.updated_at = now_secs();
        })
        .or_insert(CachedModuleEntry {
            rel,
            original_hash: hash.clone(),
            synced_hash: hash,
            updated_at: now_secs(),
        });
    save_manifest(cache_dir, db, &manifest)
}

/// Complete a reset-to-original locally: restore the working copy from the
/// given original content and record it as synced (the caller pushes the
/// same content to the server first). Returns the working file path.
pub fn apply_reset_to_original(
    cache_dir: &Path,
    db: &str,
    uri: &str,
    original_content: &str,
) -> Result<PathBuf> {
    let working = working_path_for(cache_dir, db, uri);
    if let Some(parent) = working.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to restore working copy dir: {}", parent.display()))?;
    }
    fs::write(&working, original_content)
        .with_context(|| format!("Failed to restore working copy: {}", working.display()))?;
    mark_synced(cache_dir, db, uri, original_content)?;
    Ok(working)
}

pub fn validate_new_module_uri(input: &str) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Enter a module URI, e.g. /endpoints/my/get.xqy");
    }
    if !trimmed.starts_with('/') {
        anyhow::bail!("Module URIs must start with '/', e.g. /endpoints/my/get.xqy");
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        anyhow::bail!("Module URIs must not contain '..'");
    }
    if trimmed.ends_with('/') {
        anyhow::bail!("Module URIs must name a file, not a folder");
    }
    Ok(trimmed.to_string())
}

/// Starter content for a locally created module, by file extension.
pub fn new_module_starter(uri: &str) -> &'static str {
    let ext = Path::new(uri)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "xqy" => "xquery version \"1.0-ml\";\n",
        "js" | "mjs" | "sjs" => "'use strict';\n",
        _ => "",
    }
}

/// Stage a brand-new local module: writes working + original starter copies
/// and a manifest entry whose empty `synced_hash` keeps it unsynced until
/// the first sync-all pushes it to the server. The URI must already be
/// validated with [`validate_new_module_uri`].
pub fn stage_new_module(cache_dir: &Path, db: &str, uri: &str) -> Result<PathBuf> {
    let working = working_path_for(cache_dir, db, uri);
    let original = original_path_for(cache_dir, db, uri);
    if let Some(parent) = working.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create cache dir: {}", parent.display()))?;
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create originals dir: {}", parent.display()))?;
    }
    let starter = new_module_starter(uri);
    fs::write(&original, starter)
        .with_context(|| format!("Failed to write original copy: {}", original.display()))?;
    if !working.exists() {
        fs::write(&working, starter)
            .with_context(|| format!("Failed to write working copy: {}", working.display()))?;
    }
    let mut manifest = load_manifest(cache_dir, db)?;
    let rel = uri_to_rel_path(uri).display().to_string();
    manifest.entries.insert(
        uri.to_string(),
        CachedModuleEntry {
            rel,
            original_hash: content_hash(starter),
            // Empty sentinel: never equals a real content hash, so a staged
            // module stays unsynced until its first push.
            synced_hash: String::new(),
            updated_at: now_secs(),
        },
    );
    save_manifest(cache_dir, db, &manifest)?;
    Ok(working)
}

/// Drop a locally cached module: removes working + original copies and the
/// manifest entry, pruning newly-emptied ancestor dirs (never above the db
/// root). The remote document is untouched.
pub fn drop_cached_module(cache_dir: &Path, db: &str, uri: &str) -> Result<()> {
    let root = modules_db_root(cache_dir, db);
    let working = working_path_for(cache_dir, db, uri);
    let original = original_path_for(cache_dir, db, uri);
    let _ = fs::remove_file(&working);
    let _ = fs::remove_file(&original);
    let mut manifest = load_manifest(cache_dir, db)?;
    manifest.entries.remove(uri);
    save_manifest(cache_dir, db, &manifest)?;
    for path in [&working, &original] {
        let mut dir = path.parent();
        while let Some(parent) = dir {
            if parent == root || !parent.starts_with(&root) {
                break;
            }
            if fs::remove_dir(parent).is_err() {
                break;
            }
            dir = parent.parent();
        }
    }
    Ok(())
}

/// Compute dirty/unsynced flags for every manifest entry.
/// Missing working files are reported with dirty=false, unsynced=true.
pub fn compute_statuses(
    cache_dir: &Path,
    db: &str,
    manifest: &CachedModuleManifest,
) -> Vec<CachedModuleStatus> {
    let mut statuses: Vec<CachedModuleStatus> = manifest
        .entries
        .iter()
        .map(|(uri, entry)| {
            let working_path = modules_db_root(cache_dir, db).join(&entry.rel);
            let original = original_path_for(cache_dir, db, uri);
            let missing_original = !original.exists();
            let (dirty, unsynced) = match fs::read_to_string(&working_path) {
                Ok(working_content) => {
                    let working_hash = content_hash(&working_content);
                    let dirty = !missing_original
                        && fs::read_to_string(&original)
                            .map(|o| content_hash(&o) != working_hash)
                            .unwrap_or(true);
                    let unsynced = working_hash != entry.synced_hash;
                    (dirty, unsynced)
                }
                Err(_) => (false, true),
            };
            CachedModuleStatus {
                uri: uri.clone(),
                rel: entry.rel.clone(),
                working_path,
                dirty,
                unsynced,
                missing_original,
            }
        })
        .collect();
    statuses.sort_by(|a, b| a.uri.to_lowercase().cmp(&b.uri.to_lowercase()));
    statuses
}

/// Simple line-based diff (LCS). Falls back to del+add blocks for huge files.
pub fn unified_diff(original: &str, modified: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = original.lines().collect();
    let b: Vec<&str> = modified.lines().collect();
    if a.len() * b.len() > 250_000 {
        let mut out = Vec::new();
        for line in a {
            out.push(DiffLine {
                kind: DiffKind::Del,
                text: line.to_string(),
            });
        }
        for line in b {
            out.push(DiffLine {
                kind: DiffKind::Add,
                text: line.to_string(),
            });
        }
        return out;
    }
    let n = a.len();
    let m = b.len();
    let mut dp = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            if a[i] == b[j] {
                dp[i][j] = dp[i + 1][j + 1] + 1;
            } else {
                dp[i][j] = dp[i + 1][j].max(dp[i][j + 1]);
            }
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(DiffLine {
                kind: DiffKind::Same,
                text: a[i].to_string(),
            });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(DiffLine {
                kind: DiffKind::Del,
                text: a[i].to_string(),
            });
            i += 1;
        } else {
            out.push(DiffLine {
                kind: DiffKind::Add,
                text: b[j].to_string(),
            });
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine {
            kind: DiffKind::Del,
            text: a[i].to_string(),
        });
        i += 1;
    }
    while j < m {
        out.push(DiffLine {
            kind: DiffKind::Add,
            text: b[j].to_string(),
        });
        j += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> PathBuf {
        let unique = format!(
            "ml-tui-modcache-{}-{}-{}",
            name,
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dir = std::env::temp_dir().join(unique);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn uri_to_rel_preserves_hierarchy() {
        assert_eq!(uri_to_rel_path("/a/b/c.xqy"), PathBuf::from("a/b/c.xqy"));
        assert_eq!(uri_to_rel_path("a/b/"), PathBuf::from("a/b/index"));
        assert_eq!(
            uri_to_rel_path("/../evil.xqy"),
            PathBuf::from("__parent__/evil.xqy")
        );
    }

    #[test]
    fn checkout_writes_hierarchical_copies() {
        let dir = temp_dir("checkout");
        let working = checkout_module(&dir, "Modules", "/a/b/c.xqy", "hello").unwrap();
        assert_eq!(working, dir.join("modules/Modules/a/b/c.xqy"));
        assert!(working.exists());
        assert!(dir.join("modules/Modules/.originals/a/b/c.xqy").exists());
        assert!(dir.join("modules/Modules/.manifest.json").exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dirty_and_synced_tracking() {
        let dir = temp_dir("dirty");
        checkout_module(&dir, "Modules", "/a.xqy", "v1").unwrap();
        let manifest = load_manifest(&dir, "Modules").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].dirty);
        assert!(!statuses[0].unsynced);

        fs::write(dir.join("modules/Modules/a.xqy"), "v2").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert!(statuses[0].dirty);
        assert!(statuses[0].unsynced);

        mark_synced(&dir, "Modules", "/a.xqy", "v2").unwrap();
        let manifest = load_manifest(&dir, "Modules").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert!(statuses[0].dirty); // still differs from original
        assert!(!statuses[0].unsynced);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn validate_new_module_uri_accepts_and_rejects() {
        assert_eq!(validate_new_module_uri("  /a/b.xqy ").unwrap(), "/a/b.xqy");
        assert!(validate_new_module_uri("").is_err());
        assert!(validate_new_module_uri("relative/a.xqy").is_err());
        assert!(validate_new_module_uri("/a/../evil.xqy").is_err());
        assert!(validate_new_module_uri("/a/b/").is_err());
    }

    #[test]
    fn new_module_starter_matches_extension() {
        assert!(new_module_starter("/a/b.xqy").contains("1.0-ml"));
        assert_eq!(new_module_starter("/a/b.sjs"), "'use strict';\n");
        assert_eq!(new_module_starter("/a/b.txt"), "");
    }

    #[test]
    fn staged_module_starts_unsynced_but_clean() {
        let dir = temp_dir("staged");
        let working = stage_new_module(&dir, "Modules", "/new/get.xqy").unwrap();
        assert_eq!(working, dir.join("modules/Modules/new/get.xqy"));
        assert!(working.exists());
        let manifest = load_manifest(&dir, "Modules").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].dirty);
        assert!(statuses[0].unsynced);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn drop_cached_module_removes_copies_entry_and_emptied_dirs() {
        let dir = temp_dir("drop");
        checkout_module(&dir, "Modules", "/d/e/f.xqy", "v1").unwrap();
        checkout_module(&dir, "Modules", "/d/e/g.xqy", "v1").unwrap();
        drop_cached_module(&dir, "Modules", "/d/e/f.xqy").unwrap();
        // Sibling keeps the shared ancestors alive.
        assert!(dir.join("modules/Modules/d/e").exists());
        let manifest = load_manifest(&dir, "Modules").unwrap();
        assert!(!manifest.entries.contains_key("/d/e/f.xqy"));
        assert!(manifest.entries.contains_key("/d/e/g.xqy"));
        drop_cached_module(&dir, "Modules", "/d/e/g.xqy").unwrap();
        // Last file out prunes the emptied chain, never above the db root.
        assert!(!dir.join("modules/Modules/d").exists());
        assert!(dir.join("modules/Modules").exists());
        let manifest = load_manifest(&dir, "Modules").unwrap();
        assert!(manifest.entries.is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_reset_to_original_clears_dirty_and_unsynced() {
        let dir = temp_dir("reset-apply");
        checkout_module(&dir, "Modules", "/r/get.xqy", "v1").unwrap();
        fs::write(dir.join("modules/Modules/r/get.xqy"), "edited").unwrap();
        let manifest = load_manifest(&dir, "Modules").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert!(statuses[0].dirty);
        assert!(statuses[0].unsynced);
        // Same content the caller pushes to the server.
        let working = apply_reset_to_original(&dir, "Modules", "/r/get.xqy", "v1").unwrap();
        assert_eq!(working, dir.join("modules/Modules/r/get.xqy"));
        assert_eq!(fs::read_to_string(&working).unwrap(), "v1");
        let manifest = load_manifest(&dir, "Modules").unwrap();
        let statuses = compute_statuses(&dir, "Modules", &manifest);
        assert!(!statuses[0].dirty);
        assert!(!statuses[0].unsynced);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_marks_changes() {
        let diff = unified_diff("a\nb\nc", "a\nB\nc\nd");
        assert!(
            diff.iter()
                .any(|l| l.kind == DiffKind::Del && l.text == "b")
        );
        assert!(
            diff.iter()
                .any(|l| l.kind == DiffKind::Add && l.text == "B")
        );
        assert!(
            diff.iter()
                .any(|l| l.kind == DiffKind::Add && l.text == "d")
        );
    }
}
