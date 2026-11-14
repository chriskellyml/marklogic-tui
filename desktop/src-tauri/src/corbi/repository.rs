use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, File},
    io::{ErrorKind, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

pub type Permissions = BTreeMap<String, BTreeMap<String, BTreeMap<String, bool>>>;

#[derive(Serialize)]
pub struct Catalog {
    pub root: PathBuf,
    pub projects: Vec<Project>,
    pub permissions: Permissions,
}

#[derive(Serialize)]
pub struct Project {
    name: String,
    jobs: Vec<String>,
    scripts: Vec<String>,
}

#[derive(Serialize)]
pub struct History {
    pub environment: String,
    pub id: String,
}

pub fn name(value: &str) -> Result<&str> {
    ensure!(
        !value.is_empty()
            && !value.starts_with('.')
            && !value.contains(['/', '\\', '\0'])
            && Path::new(value)
                .components()
                .all(|c| matches!(c, Component::Normal(_))),
        "Invalid CoRBi name"
    );
    Ok(value)
}

// Walk from the opened root without following links, including when creating run directories.
pub fn inside(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut path = root.to_owned();
    for part in relative.components() {
        let Component::Normal(part) = part else {
            bail!("Invalid relative path")
        };
        path.push(part);
        match path.symlink_metadata() {
            Ok(meta) => ensure!(
                !meta.is_symlink(),
                "Linked paths are unsupported: {}",
                path.display()
            ),
            Err(e) if e.kind() == ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

fn entries(path: &Path, optional: bool) -> Result<Vec<fs::DirEntry>> {
    match fs::read_dir(path) {
        Ok(entries) => {
            let mut entries = entries.collect::<std::io::Result<Vec<_>>>()?;
            entries.retain(|e| !e.file_name().to_string_lossy().starts_with('.'));
            entries.sort_by_key(|e| e.file_name());
            Ok(entries)
        }
        Err(e) if optional && e.kind() == ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("Could not read {}", path.display())),
    }
}

pub fn permissions(root: &Path) -> Result<Permissions> {
    match fs::read(inside(root, Path::new("permissions.json"))?) {
        Ok(data) => Ok(serde_json::from_slice(&data).context("Invalid permissions.json")?),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(Permissions::new()),
        Err(e) => Err(e.into()),
    }
}

fn script(path: &Path) -> bool {
    matches!(
        path.extension().and_then(OsStr::to_str),
        Some("xqy" | "sjs" | "js" | "txt")
    )
}

fn scripts(dir: &Path, prefix: &str, files: &mut Vec<String>) -> Result<()> {
    for entry in entries(dir, true)? {
        let kind = entry.file_type()?;
        let key = format!("{prefix}{}", entry.file_name().to_string_lossy());
        if kind.is_dir() {
            scripts(&entry.path(), &format!("{key}/"), files)?;
        } else if kind.is_file() && script(&entry.path()) {
            files.push(key);
        }
    }
    Ok(())
}

pub fn catalog(root: &Path) -> Result<Catalog> {
    let project_dir = inside(root, Path::new("src/projects"))?;
    let mut projects = vec![];
    for entry in entries(&project_dir, false)
        .context("Choose a CoRBi data folder containing src/projects")?
    {
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let mut jobs = vec![];
        for file in entries(&entry.path(), false)? {
            if file.file_type()?.is_file() && file.path().extension() == Some(OsStr::new("job")) {
                jobs.push(file.file_name().to_string_lossy().into_owned());
            }
        }
        let project = entry.file_name().to_string_lossy().into_owned();
        let mut project_scripts = vec![];
        scripts(
            &inside(
                root,
                &Path::new("src/projects").join(&project).join("scripts"),
            )?,
            "scripts/",
            &mut project_scripts,
        )?;
        projects.push(Project {
            name: project,
            jobs,
            scripts: project_scripts,
        });
    }
    Ok(Catalog {
        root: root.to_owned(),
        projects,
        permissions: permissions(root)?,
    })
}

pub fn source(root: &Path, project: &str, file: &str) -> Result<PathBuf> {
    let rel = Path::new(file);
    let job = rel.components().count() == 1 && file.ends_with(".job");
    ensure!(
        job || (file.starts_with("scripts/") && script(rel)),
        "Select a job or project script"
    );
    for part in file.split('/') {
        name(part)?;
    }
    let path = inside(
        root,
        &Path::new("src/projects").join(name(project)?).join(rel),
    )?;
    ensure!(path.is_file(), "Source file no longer exists");
    Ok(path)
}

pub fn history(root: &Path, project: &str) -> Result<Vec<History>> {
    let mut runs = vec![];
    for env in entries(
        &inside(root, &Path::new("runs").join(name(project)?))?,
        true,
    )? {
        if !env.file_type()?.is_dir() {
            continue;
        }
        for run in entries(&env.path(), false)? {
            if run.file_type()?.is_dir() {
                runs.push(History {
                    environment: env.file_name().to_string_lossy().into_owned(),
                    id: run.file_name().to_string_lossy().into_owned(),
                });
            }
        }
    }
    runs.sort_by(|a, b| b.id.cmp(&a.id));
    Ok(runs)
}

pub fn run_path(root: &Path, project: &str, environment: &str, id: &str) -> Result<PathBuf> {
    inside(
        root,
        &Path::new("runs")
            .join(name(project)?)
            .join(name(environment)?)
            .join(name(id)?),
    )
}

fn artifact(file: &str) -> bool {
    matches!(file, "job.options" | "wet.options")
        || file.ends_with(".log")
        || file.ends_with("report.txt")
        || file.ends_with("-DONE.txt")
}

pub fn artifacts(dir: &Path) -> Result<Vec<String>> {
    let mut files = vec![];
    for entry in entries(dir, false)? {
        let file = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_file() && artifact(&file) {
            files.push(file);
        }
    }
    scripts(&inside(dir, Path::new("scripts"))?, "scripts/", &mut files)?;
    Ok(files)
}

pub fn output(dir: &Path, file: &str, tail: bool) -> Result<String> {
    for part in file.split('/') {
        name(part)?;
    }
    ensure!(
        (!file.contains('/') && artifact(file))
            || (file.starts_with("scripts/") && script(Path::new(file))),
        "Unknown run artifact"
    );
    let mut input = File::open(inside(dir, Path::new(file))?)?;
    let bytes = input.metadata()?.len();
    let limit = 128 * 1024;
    let mut buffer = vec![];
    let truncated = bytes > limit;
    if truncated && tail {
        input.seek(SeekFrom::Start(bytes - limit))?;
    }
    input.take(limit).read_to_end(&mut buffer)?;
    let prefix = match (truncated, tail) {
        (true, true) => "[Showing the last 128 KiB]\n",
        (true, false) => "[Showing the first 128 KiB]\n",
        _ => "",
    };
    Ok(format!("{prefix}{}", String::from_utf8_lossy(&buffer)))
}

/// Create the scripts before publishing the job, without replacing any existing files.
pub fn create_job(root: &Path, project: &str, job: &str) -> Result<()> {
    use std::io::Write;
    name(project)?;
    name(job)?;
    ensure!(
        !job.ends_with(".job"),
        "Enter a job name without the .job extension"
    );
    let project_dir = inside(root, &Path::new("src/projects").join(project))?;
    fs::create_dir_all(&project_dir)?;
    let file = inside(
        root,
        &Path::new("src/projects")
            .join(project)
            .join(format!("{job}.job")),
    )?;
    ensure!(!file.exists(), "A job named {job} already exists");
    let scripts = inside(
        root,
        &Path::new("src/projects")
            .join(project)
            .join("scripts")
            .join(job),
    )?;
    fs::create_dir_all(scripts.parent().unwrap())?;
    fs::create_dir(&scripts).context("That job's scripts already exist. Choose another name.")?;
    let result = (|| -> Result<()> {
        fs::write(scripts.join("collect.xqy"), include_str!("collector.xqy"))?;
        fs::write(scripts.join("process.xqy"), include_str!("processor.xqy"))?;
        let mut output = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&file)?;
        let contents = format!(
            "URIS-MODULE=scripts/{job}/collect.xqy|ADHOC\nPROCESS-MODULE=scripts/{job}/process.xqy|ADHOC\nTHREAD-COUNT=4\n"
        );
        if let Err(e) = output.write_all(contents.as_bytes()) {
            let _ = fs::remove_file(&file);
            return Err(e.into());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(scripts);
    }
    result
}

pub fn open_default(root: &Path) -> Result<()> {
    fs::create_dir_all(root.join("src/projects"))?;
    let data = catalog(root)?;
    if data.projects.iter().all(|p| p.jobs.is_empty()) {
        create_job(root, "My jobs", "preview-documents")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_updates_job_modules_and_preserves_snapshots_and_conflicting_files() -> Result<()> {
        let root = marklogic_core::config::test_storage_dir().join("rename-corbi");
        open_default(&root)?;
        rename(&root, "My jobs", "scripts/preview-documents", "collectors")?;
        let job = source(&root, "My jobs", "preview-documents.job")?;
        let definition = fs::read_to_string(&job)?;
        assert!(definition.contains("URIS-MODULE=scripts/collectors/collect.xqy|ADHOC"));
        assert!(definition.contains("PROCESS-MODULE=scripts/collectors/process.xqy|ADHOC"));
        rename(
            &root,
            "My jobs",
            "scripts/collectors/collect.xqy",
            "find.xqy",
        )?;
        assert!(fs::read_to_string(&job)?.contains("scripts/collectors/find.xqy|ADHOC"));
        assert!(
            rename(
                &root,
                "My jobs",
                "scripts/collectors/find.xqy",
                "process.xqy"
            )
            .is_err()
        );
        assert!(rename(&root, "My jobs", "scripts/collectors", "../other").is_err());
        fs::write(
            root.join("permissions.json"),
            r#"{"My jobs":{"preview-documents.job":{"production":false}}}"#,
        )?;
        rename(&root, "My jobs", "preview-documents.job", "review.job")?;
        assert_eq!(
            permissions(&root)?["My jobs"]["review.job"]["production"],
            false
        );
        assert!(!job.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn report_preview_reads_the_start_and_log_preview_reads_the_tail() -> Result<()> {
        let root = marklogic_core::config::test_storage_dir().join("output-corbi");
        fs::create_dir_all(&root)?;
        fs::write(
            root.join("dry-report.txt"),
            format!("START\n{}\nEND", "row\n".repeat(40000)),
        )?;
        let report = output(&root, "dry-report.txt", false)?;
        let log = output(&root, "dry-report.txt", true)?;
        assert!(report.starts_with("[Showing the first 128 KiB]\nSTART"));
        assert!(!report.contains("END"));
        assert!(log.starts_with("[Showing the last 128 KiB]"));
        assert!(log.ends_with("END"));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn fresh_workspace_is_ready_and_reopening_preserves_edits() -> Result<()> {
        let root = marklogic_core::config::test_storage_dir().join("fresh-corbi");
        open_default(&root)?;
        let file = source(&root, "My jobs", "preview-documents.job")?;
        assert!(fs::read_to_string(&file)?.contains("scripts/preview-documents/collect.xqy|ADHOC"));
        let processor = source(&root, "My jobs", "scripts/preview-documents/process.xqy")?;
        fs::write(&processor, "user edits")?;
        open_default(&root)?;
        assert_eq!(fs::read_to_string(&processor)?, "user edits");
        assert!(create_job(&root, "My jobs", "preview-documents").is_err());
        assert_eq!(fs::read_to_string(&processor)?, "user edits");
        assert!(create_job(&root, "../elsewhere", "test").is_err());
        assert!(create_job(&root, "My jobs", "../test").is_err());
        create_job(&root, "Another project", "transform")?;
        assert!(source(&root, "Another project", "scripts/transform/collect.xqy")?.is_file());
        assert_eq!(catalog(&root)?.projects.len(), 2);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}

fn renamed_module_paths(contents: &str, old: &str, new: &str) -> String {
    contents
        .split_inclusive('\n')
        .map(|line| {
            let Some((key, value)) = line.split_once('=') else {
                return line.to_owned();
            };
            if key.trim_start().starts_with('#') {
                return line.to_owned();
            }
            let trimmed = value.trim_start();
            let module = trimmed.split('|').next().unwrap_or("").trim_end();
            if module != old && !module.starts_with(&format!("{old}/")) {
                return line.to_owned();
            }
            let leading = &value[..value.len() - trimmed.len()];
            format!("{key}={leading}{new}{}", &trimmed[old.len()..])
        })
        .collect()
}

pub fn rename(root: &Path, project: &str, path: &str, new_name: &str) -> Result<()> {
    name(project)?;
    name(new_name)?;
    for part in path.split('/') {
        name(part)?;
    }
    let base = inside(root, &Path::new("src/projects").join(project))?;
    let old = inside(&base, Path::new(path))?;
    ensure!(old.exists(), "The source no longer exists");
    let is_job = !path.contains('/') && path.ends_with(".job");
    ensure!(
        is_job || (path.starts_with("scripts/") && (old.is_dir() || script(&old))),
        "Select a job, script or script folder"
    );
    let new_relative = Path::new(path).with_file_name(new_name);
    if old.is_file() {
        ensure!(
            Path::new(path).extension() == new_relative.extension(),
            "Keep the file extension when renaming"
        );
    }
    let new = inside(&base, &new_relative)?;
    ensure!(!new.exists(), "That name is already in use");
    let new_path = new_relative.to_str().context("Invalid filename")?;
    let mut changes = vec![];
    for entry in entries(&base, false)? {
        if entry.file_type()?.is_file() && entry.path().extension() == Some(OsStr::new("job")) {
            let file = entry.path();
            let before = crate::workspace::read_query(&file)?;
            let after = renamed_module_paths(&before, path, new_path);
            if before != after {
                changes.push((file, before, after));
            }
        }
    }
    if is_job {
        let permission_file = inside(root, Path::new("permissions.json"))?;
        if permission_file.exists() {
            let before = crate::workspace::read_query(&permission_file)?;
            let mut rules: Permissions = serde_json::from_str(&before)?;
            if let Some(jobs) = rules.get_mut(project)
                && let Some(rule) = jobs.remove(path)
            {
                ensure!(
                    !jobs.contains_key(new_name),
                    "The new job name already has permission rules"
                );
                jobs.insert(new_name.to_owned(), rule);
                changes.push((
                    permission_file,
                    before,
                    serde_json::to_string_pretty(&rules)?,
                ));
            }
        }
    }
    fs::rename(&old, &new)?;
    let mut written = vec![];
    for (file, before, after) in &changes {
        let target = if file == &old { &new } else { file };
        if let Err(error) = crate::workspace::save_if_unchanged(target, after, before) {
            let mut rollback = vec![];
            for (file, before, after) in written.into_iter().rev() {
                if let Err(e) = crate::workspace::save_if_unchanged(file, before, after) {
                    rollback.push(e.to_string());
                }
            }
            if let Err(e) = fs::rename(&new, &old) {
                rollback.push(e.to_string());
            }
            ensure!(
                rollback.is_empty(),
                "Rename failed: {error}. Could not restore all files: {}",
                rollback.join(", ")
            );
            return Err(error);
        }
        written.push((target, before.as_str(), after.as_str()));
    }
    Ok(())
}
