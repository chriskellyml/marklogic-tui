use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SUPPORTED_EXTENSIONS: &[&str] = &["js", "mjs", "sparql", "sql", "sjs", "xqy"];

/// Smallest and largest width of the Files sidebar, including its borders.
pub const FILE_LIST_MIN_WIDTH: u16 = 15;

pub const FILE_LIST_MAX_WIDTH: u16 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryExecutionKind {
    JavaScript,
    XQuery,
    Deferred,
    Unsupported,
}

/// Every supported query file and every visible (non-hidden) folder under the
/// query root, as absolute paths sorted case-insensitively.
///
/// Query files directly in the root are not part of the tree. They are reported
/// in `loose_files` so the user can be told to move them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryTree {
    pub files: Vec<PathBuf>,
    pub folders: Vec<PathBuf>,
    pub loose_files: Vec<PathBuf>,
}

/// One visible line of the Files tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryTreeRow {
    pub path: PathBuf,
    pub is_dir: bool,
    pub depth: usize,
}

/// Where query files live when they are not kept in the launch folder.
pub fn default_query_location() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".marklogic-tui")
        .join("queries")
}

/// The default query store, created if missing and canonicalized. This is the
/// query root the app starts in.
pub fn default_query_root() -> Result<PathBuf> {
    let location = default_query_location();
    fs::create_dir_all(&location)
        .with_context(|| format!("Failed to create {}", location.display()))?;
    fs::canonicalize(&location).with_context(|| format!("Failed to resolve {}", location.display()))
}

/// True when `root` is the default query store. Top-level files are allowed
/// there; in any other folder they are reported as loose and not shown.
pub fn is_default_query_root(root: &Path) -> bool {
    default_query_root()
        .map(|default| default == root)
        .unwrap_or(false)
}

pub fn discover_query_tree(root: &Path) -> Result<QueryTree> {
    // Reading the root is the only fallible step; unreadable subfolders are skipped.
    fs::read_dir(root)
        .with_context(|| format!("Failed to read query directory: {}", root.display()))?;

    let mut tree = QueryTree::default();
    collect_query_tree(root, root, &mut tree);
    tree.files.sort_by_key(|path| sort_key(path));
    tree.folders.sort_by_key(|path| sort_key(path));
    tree.loose_files.sort_by_key(|path| sort_key(path));
    Ok(tree)
}

/// Collect supported files below `dir`, and the folders that lead to them.
/// Returns whether `dir` contains a query file at any depth below the root.
/// A folder is listed only when it does, so unrelated project folders (source,
/// build output) stay out of the tree.
fn collect_query_tree(root: &Path, dir: &Path, tree: &mut QueryTree) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    let mut has_query_files = false;
    for entry in entries.filter_map(|entry| entry.ok()) {
        if is_hidden(&entry.file_name()) {
            continue;
        }
        let path = entry.path();
        // `file_type` does not follow symlinks, so symlinked folders are not walked.
        match entry.file_type() {
            Ok(file_type) if file_type.is_dir() => {
                if collect_query_tree(root, &path, tree) {
                    tree.folders.push(path);
                    has_query_files = true;
                }
            }
            _ => {
                if is_supported_query_file(&path) && path.is_file() {
                    if dir == root {
                        tree.loose_files.push(path);
                    } else {
                        tree.files.push(path);
                        has_query_files = true;
                    }
                }
            }
        }
    }
    has_query_files
}

/// Remove folders under `root` that contain no entries at all, bottom-up so
/// that a chain of emptied folders is removed in one pass. The root itself is
/// never removed. Returns the removed folders. Best effort: a folder that
/// cannot be removed is left in place.
pub fn prune_empty_query_folders(root: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    prune_dir(root, root, &mut removed);
    removed
}

fn prune_dir(root: &Path, dir: &Path, removed: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|entry| entry.ok()) {
        if is_hidden(&entry.file_name()) {
            continue;
        }
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            prune_dir(root, &entry.path(), removed);
        }
    }

    if dir != root
        && fs::read_dir(dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false)
        && fs::remove_dir(dir).is_ok()
    {
        removed.push(dir.to_path_buf());
    }
}

/// Flatten the tree into visible rows. Folders come before files at each
/// level. Children of a folder are shown only when the folder is not in
/// `collapsed`. Entries whose parent is not a known folder are shown at the
/// top level, so no entry is silently dropped.
pub fn build_query_tree_rows(
    root: &Path,
    files: &[PathBuf],
    folders: &[PathBuf],
    collapsed: &HashSet<PathBuf>,
) -> Vec<QueryTreeRow> {
    let folder_set: HashSet<&Path> = folders.iter().map(PathBuf::as_path).collect();
    let mut children: HashMap<PathBuf, Vec<(PathBuf, bool)>> = HashMap::new();
    let entries = folders
        .iter()
        .map(|path| (path, true))
        .chain(files.iter().map(|path| (path, false)));
    for (path, is_dir) in entries {
        let parent = match path.parent() {
            Some(parent) if parent == root || folder_set.contains(parent) => parent,
            _ => root,
        };
        children
            .entry(parent.to_path_buf())
            .or_default()
            .push((path.clone(), is_dir));
    }
    for list in children.values_mut() {
        list.sort_by(|(left, left_dir), (right, right_dir)| {
            right_dir
                .cmp(left_dir)
                .then_with(|| sort_key(left).cmp(&sort_key(right)))
        });
    }

    let mut rows = Vec::new();
    push_tree_rows(root, 0, &children, collapsed, &mut rows);
    rows
}

fn push_tree_rows(
    dir: &Path,
    depth: usize,
    children: &HashMap<PathBuf, Vec<(PathBuf, bool)>>,
    collapsed: &HashSet<PathBuf>,
    rows: &mut Vec<QueryTreeRow>,
) {
    let Some(items) = children.get(dir) else {
        return;
    };
    for (path, is_dir) in items {
        rows.push(QueryTreeRow {
            path: path.clone(),
            is_dir: *is_dir,
            depth,
        });
        if *is_dir && !collapsed.contains(path) {
            push_tree_rows(path, depth + 1, children, collapsed, rows);
        }
    }
}

/// Sidebar width for a widest visible label of `content_width` columns. The
/// inner width is the label plus 2 columns for the selection marker; the two
/// border columns are added on top. Clamped to
/// [`FILE_LIST_MIN_WIDTH`, `FILE_LIST_MAX_WIDTH`].
pub fn file_list_width(content_width: usize) -> u16 {
    let wanted = u16::try_from(content_width.saturating_add(4)).unwrap_or(u16::MAX);
    wanted.clamp(FILE_LIST_MIN_WIDTH, FILE_LIST_MAX_WIDTH)
}

/// Validate a user-entered path relative to the query root, such as
/// `foo/bar/baz.xqy`. Rejects absolute paths, empty segments, `.`, `..`,
/// backslashes, and segments starting with `.` (hidden entries are not shown).
pub fn parse_relative_query_path(input: &str) -> std::result::Result<PathBuf, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("Enter a name.".to_string());
    }
    if trimmed.starts_with('/') || Path::new(trimmed).is_absolute() {
        return Err("Enter a path relative to the query folder, not an absolute path.".to_string());
    }
    if trimmed.contains('\\') || trimmed.contains('\0') {
        return Err("Use '/' to separate folders; backslashes are not allowed.".to_string());
    }

    let mut path = PathBuf::new();
    for segment in trimmed.split('/') {
        if segment.is_empty() {
            return Err("Path segments cannot be empty.".to_string());
        }
        if segment == "." || segment == ".." {
            return Err("'.' and '..' are not allowed in query paths.".to_string());
        }
        if segment.starts_with('.') {
            return Err("Names cannot start with '.' (hidden entries are not shown).".to_string());
        }
        path.push(segment);
    }
    Ok(path)
}

/// Query files must live in a folder. A bare name would be a top-level file,
/// which the tree does not show.
pub fn require_folder_for_query_file(relative: &Path) -> std::result::Result<(), String> {
    if relative.components().count() < 2 {
        return Err(format!(
            "Put query files in a folder, e.g. 'queries/{}'. Top-level files are not shown.",
            relative.display()
        ));
    }
    Ok(())
}

/// If `path` is `old` or lies under it, return the same location under `new`.
pub fn remap_path_prefix(path: &Path, old: &Path, new: &Path) -> Option<PathBuf> {
    path.strip_prefix(old).ok().map(|rest| new.join(rest))
}

fn is_hidden(name: &OsStr) -> bool {
    name.to_string_lossy().starts_with('.')
}

fn sort_key(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase()
}

pub fn is_supported_query_file(path: &Path) -> bool {
    normalized_extension(path)
        .map(|ext| SUPPORTED_EXTENSIONS.contains(&ext.as_str()))
        .unwrap_or(false)
}

pub fn query_execution_kind(path: &Path) -> QueryExecutionKind {
    match normalized_extension(path).as_deref() {
        Some("js" | "mjs" | "sjs") => QueryExecutionKind::JavaScript,
        Some("xqy") => QueryExecutionKind::XQuery,
        Some("sql" | "sparql") => QueryExecutionKind::Deferred,
        Some(_) | None => QueryExecutionKind::Unsupported,
    }
}

pub fn load_query_file(path: &Path) -> Result<String> {
    fs::read_to_string(path)
        .with_context(|| format!("Failed to read query file: {}", path.display()))
}

pub fn save_query_file(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("Failed to save query file: {}", path.display()))
}

pub fn display_query_path(path: &Path, root_dir: &Path) -> String {
    path.strip_prefix(root_dir)
        .unwrap_or(path)
        .display()
        .to_string()
}

pub fn editor_lines(text: &str) -> Vec<String> {
    text.split('\n').map(String::from).collect()
}

pub fn should_autosave(
    is_dirty: bool,
    last_edit_at: Option<Instant>,
    now: Instant,
    interval: Duration,
) -> bool {
    is_dirty
        && last_edit_at
            .map(|last_edit_at| now.duration_since(last_edit_at) >= interval)
            .unwrap_or(false)
}

fn normalized_extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|ext| ext.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn temp_test_dir(name: &str) -> PathBuf {
        let unique = format!(
            "marklogic-tui-{}-{}-{}",
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
    fn discover_query_files_filters_and_sorts_supported_extensions() {
        let dir = temp_test_dir("discover");
        fs::create_dir_all(&dir).unwrap();

        fs::write(dir.join("z-last.sql"), "SELECT 1").unwrap();
        fs::write(dir.join("a-first.XQY"), "xquery version \"1.0-ml\";").unwrap();
        fs::write(dir.join("middle.txt"), "ignore me").unwrap();
        fs::create_dir_all(dir.join("nested.js")).unwrap();
        fs::write(dir.join("b-second.sjs"), "'use strict';").unwrap();

        // Top-level files are reported as loose; this test checks filtering and sorting.
        let files = discover_query_tree(&dir).unwrap().loose_files;
        let names = files
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>();

        assert_eq!(names, vec!["a-first.XQY", "b-second.sjs", "z-last.sql"]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_execution_kind_is_case_insensitive() {
        assert_eq!(
            query_execution_kind(Path::new("query.SJS")),
            QueryExecutionKind::JavaScript
        );
        assert_eq!(
            query_execution_kind(Path::new("query.xqy")),
            QueryExecutionKind::XQuery
        );
        assert_eq!(
            query_execution_kind(Path::new("query.SPARQL")),
            QueryExecutionKind::Deferred
        );
        assert_eq!(
            query_execution_kind(Path::new("query.txt")),
            QueryExecutionKind::Unsupported
        );
    }

    #[test]
    fn should_autosave_uses_idle_cooldown() {
        let now = Instant::now();

        assert!(!should_autosave(
            false,
            Some(now - Duration::from_secs(3)),
            now,
            Duration::from_secs(2)
        ));
        assert!(!should_autosave(
            true,
            Some(now - Duration::from_secs(1)),
            now,
            Duration::from_secs(2)
        ));
        assert!(should_autosave(
            true,
            Some(now - Duration::from_secs(3)),
            now,
            Duration::from_secs(2)
        ));
    }

    fn write_file(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "1").unwrap();
    }

    #[test]
    fn discover_query_tree_recurses_and_skips_hidden_entries() {
        let dir = temp_test_dir("tree");
        write_file(&dir.join("a/b/nested.sjs"));
        write_file(&dir.join("a/notes.txt"));
        write_file(&dir.join(".hidden/secret.xqy"));
        write_file(&dir.join("a/.env.xqy"));
        write_file(&dir.join(".marklogic-tui/cache.xqy"));
        fs::create_dir_all(dir.join("empty")).unwrap();

        let tree = discover_query_tree(&dir).unwrap();
        let files: Vec<String> = tree
            .files
            .iter()
            .map(|p| display_query_path(p, &dir))
            .collect();
        let folders: Vec<String> = tree
            .folders
            .iter()
            .map(|p| display_query_path(p, &dir))
            .collect();

        assert_eq!(files, vec!["a/b/nested.sjs"]);
        assert_eq!(folders, vec!["a", "a/b"]);
        assert!(tree.loose_files.is_empty());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discover_query_tree_reports_top_level_files_as_loose_and_hides_them() {
        let dir = temp_test_dir("tree-loose");
        write_file(&dir.join("top.xqy"));
        write_file(&dir.join("notes.txt"));
        write_file(&dir.join("sub/inner.xqy"));

        let tree = discover_query_tree(&dir).unwrap();

        assert_eq!(tree.files, vec![dir.join("sub/inner.xqy")]);
        assert_eq!(tree.loose_files, vec![dir.join("top.xqy")]);
        assert_eq!(tree.folders, vec![dir.join("sub")]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn discover_query_tree_skips_folders_without_query_files() {
        let dir = temp_test_dir("tree-code");
        write_file(&dir.join("src/lib.rs"));
        write_file(&dir.join("src/deep/mod.rs"));
        write_file(&dir.join("target/debug/out.txt"));
        write_file(&dir.join("queries/q.xqy"));

        let tree = discover_query_tree(&dir).unwrap();

        assert_eq!(tree.files, vec![dir.join("queries/q.xqy")]);
        assert_eq!(tree.folders, vec![dir.join("queries")]);

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn require_folder_for_query_file_rejects_bare_names() {
        assert!(require_folder_for_query_file(Path::new("a.xqy")).is_err());
        assert!(require_folder_for_query_file(Path::new("sub/a.xqy")).is_ok());
        assert!(require_folder_for_query_file(Path::new("x/y/a.xqy")).is_ok());
    }

    #[test]
    fn temp_query_roots_are_not_the_default_store() {
        let dir = temp_test_dir("not-default");
        fs::create_dir_all(&dir).unwrap();
        assert!(!is_default_query_root(&dir));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_query_location_is_under_home_marklogic_tui() {
        let location = default_query_location();
        assert!(location.ends_with(".marklogic-tui/queries"));
        if let Some(home) = dirs::home_dir() {
            assert!(location.starts_with(home));
        }
    }

    #[test]
    fn discover_query_tree_fails_when_root_is_missing() {
        let dir = temp_test_dir("tree-missing");
        assert!(discover_query_tree(&dir).is_err());
    }

    #[test]
    fn prune_removes_empty_chains_but_keeps_root_and_non_empty_folders() {
        let dir = temp_test_dir("prune");
        fs::create_dir_all(dir.join("chain/one/two")).unwrap();
        write_file(&dir.join("keep/notes.txt"));
        fs::create_dir_all(dir.join("keep/empty-child")).unwrap();
        fs::create_dir_all(dir.join(".hidden-dir")).unwrap();

        let removed = prune_empty_query_folders(&dir);

        let mut removed: Vec<String> = removed
            .iter()
            .map(|p| display_query_path(p, &dir))
            .collect();
        removed.sort();
        assert_eq!(
            removed,
            vec!["chain", "chain/one", "chain/one/two", "keep/empty-child"]
        );
        assert!(dir.exists());
        assert!(dir.join("keep/notes.txt").exists());
        assert!(dir.join(".hidden-dir").exists());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn query_tree_rows_put_folders_first_and_hide_collapsed_children() {
        let root = PathBuf::from("/q");
        let files = vec![
            root.join("zeta.xqy"),
            root.join("docs/readme.sjs"),
            root.join("docs/deep/x.xqy"),
            root.join("alpha.xqy"),
        ];
        let folders = vec![root.join("docs"), root.join("docs/deep")];

        let rows = build_query_tree_rows(&root, &files, &folders, &HashSet::new());
        let shape: Vec<(String, bool, usize)> = rows
            .iter()
            .map(|r| (display_query_path(&r.path, &root), r.is_dir, r.depth))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("docs".to_string(), true, 0),
                ("docs/deep".to_string(), true, 1),
                ("docs/deep/x.xqy".to_string(), false, 2),
                ("docs/readme.sjs".to_string(), false, 1),
                ("alpha.xqy".to_string(), false, 0),
                ("zeta.xqy".to_string(), false, 0),
            ]
        );

        let collapsed: HashSet<PathBuf> = [root.join("docs")].into_iter().collect();
        let rows = build_query_tree_rows(&root, &files, &folders, &collapsed);
        let names: Vec<String> = rows
            .iter()
            .map(|r| display_query_path(&r.path, &root))
            .collect();
        assert_eq!(names, vec!["docs", "alpha.xqy", "zeta.xqy"]);
    }

    #[test]
    fn query_tree_rows_keep_entries_whose_parent_is_unknown() {
        let root = PathBuf::from("/q");
        let files = vec![PathBuf::from("/elsewhere/a.xqy")];
        let rows = build_query_tree_rows(&root, &files, &[], &HashSet::new());
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].depth, 0);
    }

    #[test]
    fn parse_relative_query_path_accepts_nested_names() {
        assert_eq!(
            parse_relative_query_path("foo/foo/foo/blah.xqy").unwrap(),
            PathBuf::from("foo/foo/foo/blah.xqy")
        );
        assert_eq!(
            parse_relative_query_path("  foo-bar.xqy  ").unwrap(),
            PathBuf::from("foo-bar.xqy")
        );
    }

    #[test]
    fn parse_relative_query_path_rejects_unsafe_or_invalid_paths() {
        for bad in [
            "",
            "   ",
            "/abs/x.xqy",
            "a//b.xqy",
            "a/",
            "a/../b.xqy",
            "./a.xqy",
            "a/.hidden.xqy",
            ".hidden/a.xqy",
            "a\\b.xqy",
        ] {
            assert!(parse_relative_query_path(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn remap_path_prefix_only_matches_whole_components() {
        let old = Path::new("/q/foo");
        let new = Path::new("/q/bar/baz");
        assert_eq!(
            remap_path_prefix(Path::new("/q/foo"), old, new),
            Some(PathBuf::from("/q/bar/baz"))
        );
        assert_eq!(
            remap_path_prefix(Path::new("/q/foo/a.xqy"), old, new),
            Some(PathBuf::from("/q/bar/baz/a.xqy"))
        );
        assert_eq!(
            remap_path_prefix(Path::new("/q/foobar/a.xqy"), old, new),
            None
        );
    }

    #[test]
    fn file_list_width_is_clamped_between_15_and_100() {
        assert_eq!(file_list_width(0), 15);
        assert_eq!(file_list_width(5), 15);
        assert_eq!(file_list_width(30), 34);
        assert_eq!(file_list_width(96), 100);
        assert_eq!(file_list_width(500), 100);
    }
}
