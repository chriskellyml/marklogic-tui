mod repository;
mod runner;
mod runtime;

use crate::workspace::{read_query, save_if_unchanged};
use anyhow::{Context, Result, ensure};
use repository::{Catalog, History};
use runner::{Request, Run, Runner};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};
use tauri::State;

#[derive(Default)]
pub struct CorbiState(Mutex<Session>);

#[derive(Default)]
struct Session {
    root: Option<PathBuf>,
    runner: Runner,
}

impl Session {
    fn root(&self, expected: &Path) -> Result<&Path> {
        ensure!(
            self.root.as_deref() == Some(expected),
            "The CoRBi data folder changed"
        );
        self.root.as_deref().context("Open a CoRBi data folder")
    }
}

impl CorbiState {
    fn with<T>(&self, op: impl FnOnce(&mut Session) -> Result<T>) -> Result<T, String> {
        let mut session = self.0.lock().map_err(|_| "CoRBi workspace unavailable")?;
        op(&mut session).map_err(|e| format!("{e:#}"))
    }

    pub fn shutdown(&self) {
        let _ = self.with(|session| session.runner.stop());
    }
}

#[tauri::command]
pub async fn corbi_open(
    state: State<'_, CorbiState>,
    path: Option<PathBuf>,
) -> Result<Option<Catalog>, String> {
    let path = match path.or_else(|| std::env::var_os("CORBI_DATA_DIR").map(PathBuf::from)) {
        Some(path) => path,
        None => {
            let path = runtime::default_root();
            state.with(|_| repository::open_default(&path))?;
            path
        }
    };
    state.with(|session| {
        let path = path
            .canonicalize()
            .context("Could not open CoRBi data folder")?;
        let data = repository::catalog(&path)?;
        if session.root.as_ref() != Some(&path) {
            ensure!(
                !session
                    .runner
                    .status()?
                    .is_some_and(|r| r.status == "running"),
                "Stop the active run before changing data folders"
            );
            session.runner = Runner::default();
            session.root = Some(path);
        }
        Ok(Some(data))
    })
}

#[tauri::command]
pub async fn corbi_create_job(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    name: String,
) -> Result<Catalog, String> {
    state.with(|s| {
        let root = s.root(&root)?;
        repository::create_job(root, &project, &name)?;
        repository::catalog(root)
    })
}

#[tauri::command]
pub async fn corbi_rename(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    path: String,
    name: String,
) -> Result<Catalog, String> {
    state.with(|s| {
        ensure!(
            !s.runner.status()?.is_some_and(|r| r.status == "running"),
            "Stop the active run before renaming"
        );
        repository::rename(s.root(&root)?, &project, &path, &name)?;
        repository::catalog(&root)
    })
}

#[tauri::command]
pub async fn corbi_source(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    file: String,
) -> Result<String, String> {
    state.with(|s| read_query(&repository::source(s.root(&root)?, &project, &file)?))
}

#[tauri::command]
pub async fn corbi_save(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    file: String,
    contents: String,
    expected: String,
) -> Result<(), String> {
    state.with(|s| {
        save_if_unchanged(
            &repository::source(s.root(&root)?, &project, &file)?,
            &contents,
            &expected,
        )
    })
}

#[tauri::command]
pub async fn corbi_history(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
) -> Result<Vec<History>, String> {
    state.with(|s| repository::history(s.root(&root)?, &project))
}

#[tauri::command]
pub async fn corbi_artifacts(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    environment: String,
    id: String,
) -> Result<Vec<String>, String> {
    state.with(|s| {
        repository::artifacts(&repository::run_path(
            s.root(&root)?,
            &project,
            &environment,
            &id,
        )?)
    })
}

#[tauri::command]
pub async fn corbi_output(
    state: State<'_, CorbiState>,
    root: PathBuf,
    project: String,
    environment: String,
    id: String,
    file: String,
    tail: Option<bool>,
) -> Result<String, String> {
    state.with(|s| {
        repository::output(
            &repository::run_path(s.root(&root)?, &project, &environment, &id)?,
            &file,
            tail.unwrap_or(true),
        )
    })
}

#[tauri::command]
pub async fn corbi_run(
    state: State<'_, CorbiState>,
    root: PathBuf,
    request: Request,
) -> Result<Run, String> {
    state.with(|s| {
        s.root(&root)?;
        Ok(())
    })?;
    let libraries = runtime::libraries(&root)
        .await
        .map_err(|e| format!("{e:#}"))?;
    state.with(|s| {
        s.root(&root)?;
        s.runner.start(&root, request, &libraries)
    })
}

#[tauri::command]
pub async fn corbi_status(
    state: State<'_, CorbiState>,
    root: PathBuf,
) -> Result<Option<Run>, String> {
    state.with(|s| {
        s.root(&root)?;
        s.runner.status()
    })
}

#[tauri::command]
pub async fn corbi_stop(
    state: State<'_, CorbiState>,
    root: PathBuf,
    id: String,
) -> Result<(), String> {
    state.with(|s| {
        s.root(&root)?;
        ensure!(
            s.runner.status()?.is_some_and(|r| r.id == id),
            "The active run changed"
        );
        s.runner.stop()
    })
}
