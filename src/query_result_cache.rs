use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

const SNAPSHOT_FILE_NAME: &str = "results-v1.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueryResultSnapshot {
    pub query_results: Vec<String>,
    pub selected_index: Option<usize>,
    #[serde(default)]
    pub created_at: Option<u64>,
}

pub fn load_query_result_snapshot(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    query_path: &Path,
) -> Result<Option<QueryResultSnapshot>> {
    let snapshot_path = query_snapshot_path(cache_root_dir, query_root_dir, query_path)?;
    if !snapshot_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&snapshot_path)
        .with_context(|| format!("Failed to read result cache: {}", snapshot_path.display()))?;
    let mut snapshot: QueryResultSnapshot = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse result cache: {}", snapshot_path.display()))?;
    if snapshot.created_at.is_none() {
        snapshot.created_at = fs::metadata(&snapshot_path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
    }
    Ok(Some(snapshot))
}

pub fn save_query_result_snapshot(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    query_path: &Path,
    snapshot: &QueryResultSnapshot,
) -> Result<()> {
    let cache_dir = query_cache_dir(cache_root_dir, query_root_dir, query_path)?;
    fs::create_dir_all(&cache_dir).with_context(|| {
        format!(
            "Failed to create query result cache directory: {}",
            cache_dir.display()
        )
    })?;

    let snapshot_path = cache_dir.join(SNAPSHOT_FILE_NAME);
    let serialized = serde_json::to_string_pretty(snapshot)
        .context("Failed to serialize query result snapshot")?;
    fs::write(&snapshot_path, serialized)
        .with_context(|| format!("Failed to write result cache: {}", snapshot_path.display()))
}

pub fn remove_query_result_cache(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    query_path: &Path,
) -> Result<()> {
    let cache_dir = query_cache_dir(cache_root_dir, query_root_dir, query_path)?;
    if cache_dir.exists() {
        fs::remove_dir_all(&cache_dir).with_context(|| {
            format!("Failed to remove cache directory: {}", cache_dir.display())
        })?;
    }
    Ok(())
}

pub fn move_query_result_cache(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    old_query_path: &Path,
    new_query_path: &Path,
) -> Result<()> {
    let old_cache_dir = query_cache_dir(cache_root_dir, query_root_dir, old_query_path)?;
    if !old_cache_dir.exists() {
        return Ok(());
    }

    let new_cache_dir = query_cache_dir(cache_root_dir, query_root_dir, new_query_path)?;
    if let Some(parent_dir) = new_cache_dir.parent() {
        fs::create_dir_all(parent_dir).with_context(|| {
            format!(
                "Failed to create parent cache directory: {}",
                parent_dir.display()
            )
        })?;
    }

    if new_cache_dir.exists() {
        fs::remove_dir_all(&new_cache_dir).with_context(|| {
            format!(
                "Failed to remove existing destination cache directory: {}",
                new_cache_dir.display()
            )
        })?;
    }

    fs::rename(&old_cache_dir, &new_cache_dir).with_context(|| {
        format!(
            "Failed to move cache directory from {} to {}",
            old_cache_dir.display(),
            new_cache_dir.display()
        )
    })?;
    Ok(())
}

fn query_snapshot_path(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    query_path: &Path,
) -> Result<PathBuf> {
    Ok(query_cache_dir(cache_root_dir, query_root_dir, query_path)?.join(SNAPSHOT_FILE_NAME))
}

fn query_cache_dir(
    cache_root_dir: &Path,
    query_root_dir: &Path,
    query_path: &Path,
) -> Result<PathBuf> {
    let relative_query_path = relative_query_path(query_root_dir, query_path)?;

    if relative_query_path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        bail!(
            "Query path cannot be mapped to cache directory: {}",
            query_path.display()
        );
    }

    Ok(cache_root_dir.join(relative_query_path))
}

fn relative_query_path(query_root_dir: &Path, query_path: &Path) -> Result<PathBuf> {
    if query_path.is_absolute() {
        return query_path
            .strip_prefix(query_root_dir)
            .map(Path::to_path_buf)
            .with_context(|| {
                format!(
                    "Query path {} is outside query root {}",
                    query_path.display(),
                    query_root_dir.display()
                )
            });
    }

    Ok(query_path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-cache-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    fn test_snapshot() -> QueryResultSnapshot {
        QueryResultSnapshot {
            query_results: vec!["one".to_string(), "two".to_string()],
            selected_index: Some(1),
            created_at: Some(1),
        }
    }

    #[test]
    fn save_and_load_snapshot_round_trip() {
        let root_dir = temp_test_dir("round-trip");
        fs::create_dir_all(&root_dir).unwrap();

        let query_path = root_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();
        let cache_dir = root_dir.join(".marklogic-tui");
        let snapshot = test_snapshot();

        save_query_result_snapshot(&cache_dir, &root_dir, &query_path, &snapshot).unwrap();
        let loaded = load_query_result_snapshot(&cache_dir, &root_dir, &query_path).unwrap();

        assert_eq!(loaded, Some(snapshot));
        assert!(
            cache_dir
                .join("query-1.xqy")
                .join(SNAPSHOT_FILE_NAME)
                .exists()
        );

        fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn load_snapshot_returns_none_when_missing() {
        let root_dir = temp_test_dir("missing");
        fs::create_dir_all(&root_dir).unwrap();

        let query_path = root_dir.join("query-1.xqy");
        let cache_dir = root_dir.join(".marklogic-tui");

        let loaded = load_query_result_snapshot(&cache_dir, &root_dir, &query_path).unwrap();
        assert!(loaded.is_none());

        fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn load_snapshot_without_created_at_uses_file_mtime() {
        let root_dir = temp_test_dir("missing-created-at");
        fs::create_dir_all(&root_dir).unwrap();

        let query_path = root_dir.join("query-1.xqy");
        fs::write(&query_path, "xquery version \"1.0-ml\";").unwrap();
        let cache_dir = root_dir.join(".marklogic-tui");
        let snapshot_dir = cache_dir.join("query-1.xqy");
        fs::create_dir_all(&snapshot_dir).unwrap();
        let snapshot_path = snapshot_dir.join(SNAPSHOT_FILE_NAME);
        fs::write(
            &snapshot_path,
            "{\n  \"query_results\": [\"one\"],\n  \"selected_index\": 0\n}",
        )
        .unwrap();

        let loaded = load_query_result_snapshot(&cache_dir, &root_dir, &query_path)
            .unwrap()
            .unwrap();
        assert!(loaded.created_at.is_some());

        fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn move_query_cache_tracks_renamed_query_file() {
        let root_dir = temp_test_dir("move");
        fs::create_dir_all(&root_dir).unwrap();

        let old_query_path = root_dir.join("query-1.xqy");
        let new_query_path = root_dir.join("query-final.xqy");
        let cache_dir = root_dir.join(".marklogic-tui");
        let snapshot = test_snapshot();

        save_query_result_snapshot(&cache_dir, &root_dir, &old_query_path, &snapshot).unwrap();
        move_query_result_cache(&cache_dir, &root_dir, &old_query_path, &new_query_path).unwrap();

        assert!(!cache_dir.join("query-1.xqy").exists());
        let loaded = load_query_result_snapshot(&cache_dir, &root_dir, &new_query_path).unwrap();
        assert_eq!(loaded, Some(snapshot));

        fs::remove_dir_all(root_dir).unwrap();
    }

    #[test]
    fn remove_query_cache_deletes_snapshot_directory() {
        let root_dir = temp_test_dir("remove");
        fs::create_dir_all(&root_dir).unwrap();

        let query_path = root_dir.join("query-1.xqy");
        let cache_dir = root_dir.join(".marklogic-tui");
        let snapshot = test_snapshot();

        save_query_result_snapshot(&cache_dir, &root_dir, &query_path, &snapshot).unwrap();
        remove_query_result_cache(&cache_dir, &root_dir, &query_path).unwrap();

        assert!(!cache_dir.join("query-1.xqy").exists());
        let loaded = load_query_result_snapshot(&cache_dir, &root_dir, &query_path).unwrap();
        assert!(loaded.is_none());

        fs::remove_dir_all(root_dir).unwrap();
    }
}
