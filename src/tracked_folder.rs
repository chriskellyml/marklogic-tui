use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrackedFolderEntry {
    pub path: String,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub last_accessed: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TrackedFolderStore {
    #[serde(default)]
    pub folders: Vec<TrackedFolderEntry>,
}

impl TrackedFolderStore {
    pub fn storage_dir() -> PathBuf {
        #[cfg(test)]
        {
            let dir = crate::config::test_storage_dir().join("folders");
            fs::create_dir_all(&dir).expect("create isolated test folder directory");
            dir
        }
        #[cfg(not(test))]
        {
            let dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".marklogic-tui");
            fs::create_dir_all(&dir).ok();
            dir
        }
    }

    pub fn storage_path() -> PathBuf {
        Self::storage_dir().join("folders.toml")
    }

    pub fn load() -> Result<Self> {
        Self::load_from_path(&Self::storage_path())
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if path.exists() {
            let content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read tracked folders: {}", path.display()))?;
            let mut store: Self = toml::from_str(&content)
                .with_context(|| format!("Failed to parse tracked folders: {}", path.display()))?;
            store.normalize();
            Ok(store)
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to_path(&Self::storage_path())
    }

    pub fn save_to_path(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create tracked-folder directory: {}",
                    parent.display()
                )
            })?;
        }

        let mut store = self.clone();
        store.normalize();
        let content =
            toml::to_string_pretty(&store).context("Failed to serialize tracked folders")?;
        fs::write(path, content)
            .with_context(|| format!("Failed to write tracked folders: {}", path.display()))
    }

    pub fn add_folder(&mut self, path: &Path) -> Result<PathBuf> {
        let canonical_path = canonicalize_folder(path)?;
        let path_string = canonical_path.to_string_lossy().to_string();
        if !self.folders.iter().any(|entry| entry.path == path_string) {
            self.folders.push(TrackedFolderEntry {
                path: path_string,
                favorite: false,
                last_accessed: None,
            });
        }
        self.normalize();
        Ok(canonical_path)
    }

    pub fn record_access(&mut self, path: &Path) -> Result<PathBuf> {
        let canonical_path = canonicalize_folder(path)?;
        let path_string = canonical_path.to_string_lossy().to_string();
        let last_accessed = Some(current_unix_timestamp());

        if let Some(entry) = self
            .folders
            .iter_mut()
            .find(|entry| entry.path == path_string)
        {
            entry.last_accessed = last_accessed;
        } else {
            self.folders.push(TrackedFolderEntry {
                path: path_string,
                favorite: false,
                last_accessed,
            });
        }

        self.normalize();
        Ok(canonical_path)
    }

    pub fn toggle_favorite(&mut self, path: &Path) -> Option<bool> {
        let path_string = path.to_string_lossy().to_string();
        let entry = self
            .folders
            .iter_mut()
            .find(|entry| entry.path == path_string)?;
        entry.favorite = !entry.favorite;
        Some(entry.favorite)
    }

    pub fn remove_folder(&mut self, path: &Path) -> bool {
        let path_string = path.to_string_lossy().to_string();
        let before = self.folders.len();
        self.folders.retain(|entry| entry.path != path_string);
        before != self.folders.len()
    }

    pub fn favorite_entries(&self) -> Vec<TrackedFolderEntry> {
        let mut entries = self
            .folders
            .iter()
            .filter(|entry| entry.favorite)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| compare_path_strings(&left.path, &right.path));
        entries
    }

    pub fn recent_entries(&self) -> Vec<TrackedFolderEntry> {
        let mut entries = self
            .folders
            .iter()
            .filter(|entry| !entry.favorite)
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .last_accessed
                .cmp(&left.last_accessed)
                .then_with(|| compare_path_strings(&left.path, &right.path))
        });
        entries
    }

    fn normalize(&mut self) {
        let mut deduped: Vec<TrackedFolderEntry> = Vec::new();

        for entry in self.folders.drain(..) {
            let path = entry.path.trim();
            if path.is_empty() {
                continue;
            }

            if let Some(existing) = deduped.iter_mut().find(|existing| existing.path == path) {
                existing.favorite |= entry.favorite;
                existing.last_accessed = existing.last_accessed.max(entry.last_accessed);
            } else {
                deduped.push(TrackedFolderEntry {
                    path: path.to_string(),
                    favorite: entry.favorite,
                    last_accessed: entry.last_accessed,
                });
            }
        }

        self.folders = deduped;
    }
}

pub fn canonicalize_folder(path: &Path) -> Result<PathBuf> {
    let canonical_path = fs::canonicalize(path)
        .with_context(|| format!("Failed to access folder: {}", path.display()))?;
    if !canonical_path.is_dir() {
        bail!("Folder is not a directory: {}", canonical_path.display());
    }
    Ok(canonical_path)
}

fn current_unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn compare_path_strings(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase().cmp(&right.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::{TrackedFolderEntry, TrackedFolderStore, canonicalize_folder};
    use std::fs;
    use std::path::PathBuf;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-folders-{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn save_and_load_round_trip_keeps_folder_metadata() {
        let dir = temp_test_dir("round-trip");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("folders.toml");

        let store = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: "/tmp/project".to_string(),
                favorite: true,
                last_accessed: Some(42),
            }],
        };

        store.save_to_path(&path).unwrap();
        let loaded = TrackedFolderStore::load_from_path(&path).unwrap();

        assert_eq!(loaded.folders, store.folders);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn normalize_merges_duplicate_entries() {
        let dir = temp_test_dir("normalize");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("folders.toml");
        fs::write(
            &path,
            r#"[[folders]]
path = "/tmp/project"

[[folders]]
path = "/tmp/project"
favorite = true
last_accessed = 7
"#,
        )
        .unwrap();

        let loaded = TrackedFolderStore::load_from_path(&path).unwrap();

        assert_eq!(loaded.folders.len(), 1);
        assert!(loaded.folders[0].favorite);
        assert_eq!(loaded.folders[0].last_accessed, Some(7));

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn favorites_and_recents_are_split_into_sections() {
        let store = TrackedFolderStore {
            folders: vec![
                TrackedFolderEntry {
                    path: "/tmp/c".to_string(),
                    favorite: false,
                    last_accessed: Some(1),
                },
                TrackedFolderEntry {
                    path: "/tmp/a".to_string(),
                    favorite: true,
                    last_accessed: Some(2),
                },
                TrackedFolderEntry {
                    path: "/tmp/b".to_string(),
                    favorite: false,
                    last_accessed: Some(4),
                },
            ],
        };

        let favorites = store.favorite_entries();
        let recents = store.recent_entries();

        assert_eq!(favorites.len(), 1);
        assert_eq!(favorites[0].path, "/tmp/a");
        assert_eq!(
            recents
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            vec!["/tmp/b", "/tmp/c"]
        );
    }

    #[test]
    fn record_access_tracks_last_accessed_for_existing_folder() {
        let dir = temp_test_dir("record-access");
        fs::create_dir_all(&dir).unwrap();
        let canonical_dir = canonicalize_folder(&dir).unwrap();
        let canonical_string = canonical_dir.to_string_lossy().to_string();
        let mut store = TrackedFolderStore {
            folders: vec![TrackedFolderEntry {
                path: canonical_string.clone(),
                favorite: true,
                last_accessed: Some(1),
            }],
        };

        store.record_access(&dir).unwrap();

        assert_eq!(store.folders.len(), 1);
        assert_eq!(store.folders[0].path, canonical_string);
        assert!(store.folders[0].favorite);
        assert!(store.folders[0].last_accessed.unwrap() >= 1);

        fs::remove_dir_all(dir).unwrap();
    }
}
