//! Load and prepare local history without blocking the terminal event loop.
use super::*;
use std::sync::atomic::AtomicUsize;
use std::sync::mpsc::{self as channel, TryRecvError};

#[derive(Clone, Copy)]
pub(super) enum Stage {
    Catalog,
    Archives,
    Preparing,
}

pub(super) struct Progress {
    phase: AtomicUsize,
    pub files: AtomicU64,
    cancelled: AtomicBool,
}

impl Progress {
    pub fn check_cancelled(&self) -> Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            bail!("Loading local history cancelled");
        }
        Ok(())
    }

    pub fn stage(&self, stage: Stage) -> Result<()> {
        self.check_cancelled()?;
        self.phase.store(stage as usize, Ordering::Relaxed);
        Ok(())
    }
}

pub(super) struct CacheLoadJob {
    rx: channel::Receiver<Result<Box<LogViewer>, String>>,
    progress: Arc<Progress>,
    started: Instant,
}

impl Drop for CacheLoadJob {
    fn drop(&mut self) {
        self.progress.cancelled.store(true, Ordering::Relaxed);
    }
}

impl LogViewer {
    pub(crate) fn cache_loading(&self) -> bool {
        self.cache_load.is_some()
    }

    pub(super) fn start_cache_load(&mut self) {
        let mut worker = LogViewer {
            cache_root: self.cache_root.clone(),
            identity: self.identity.clone(),
            directory: self.directory.clone(),
            window: self.window,
            undo_window: self.undo_window,
            window_initialized: self.window_initialized,
            cursor_time: self.cursor_time,
            scope_days: self.scope_days,
            clock: self.clock,
            server_offset: self.server_offset,
            ..LogViewer::default()
        };
        let progress = Arc::new(Progress {
            phase: AtomicUsize::new(Stage::Catalog as usize),
            files: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        });
        let (tx, rx) = channel::channel();
        self.cache_load = Some(CacheLoadJob {
            rx,
            progress: progress.clone(),
            started: Instant::now(),
        });
        self.busy = true;
        self.message = "Loading local log history…".into();
        std::thread::spawn(move || {
            let result = worker
                .load_cache_with_progress(Some(&progress))
                .and_then(|()| progress.check_cancelled())
                .map(|()| Box::new(worker))
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(result);
        });
    }

    pub(crate) fn cache_loading_status(&self) -> Option<String> {
        let job = self.cache_load.as_ref()?;
        let elapsed = job.started.elapsed();
        let spinner = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]
            [(elapsed.as_millis() / 100 % 10) as usize];
        let phase = match job.progress.phase.load(Ordering::Relaxed) {
            n if n == Stage::Archives as usize => "Checking retained log archives",
            n if n == Stage::Preparing as usize => "Preparing the initial log view",
            _ => "Reading local log catalog and cache metadata",
        };
        Some(format!(
            "{spinner} {phase} · {} files checked · {}s",
            job.progress.files.load(Ordering::Relaxed),
            elapsed.as_secs()
        ))
    }

    pub(super) fn poll_cache_load(&mut self, rt: &Runtime) {
        let result = match self.cache_load.as_ref().unwrap().rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Cache loader stopped unexpectedly".into()),
        };
        self.cache_load.take();
        match result {
            Ok(mut loaded) => {
                loaded.client = self.client.take();
                loaded.opened_from = self.opened_from.take();
                loaded.truncation_dialog = self.truncation_dialog.take();
                loaded.truncation_draft = self.truncation_draft.take();
                loaded.environment_indicator = std::mem::take(&mut self.environment_indicator);
                *self = *loaded;
                if let Some(client) = self.client.clone() {
                    self.discover_cached_hosts(client, rt);
                }
            }
            Err(error) => {
                self.busy = false;
                self.message = format!("Loading local history: {error}. Run :logs to retry.");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_cache_load_rejects_its_late_result() {
        let (tx, rx) = channel::channel();
        let progress = Arc::new(Progress {
            phase: AtomicUsize::new(Stage::Catalog as usize),
            files: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
        });
        let mut logs = LogViewer {
            cache_load: Some(CacheLoadJob {
                rx,
                progress: progress.clone(),
                started: Instant::now(),
            }),
            cache_load_needed: true,
            busy: true,
            ..LogViewer::default()
        };
        logs.cancel();
        assert!(!logs.cache_loading());
        assert!(!logs.busy);
        assert!(
            logs.cache_load_needed,
            "a later open must retry the cancelled load"
        );
        assert!(progress.check_cancelled().is_err());
        assert!(
            tx.send(Ok(Box::new(LogViewer::default()))).is_err(),
            "late data must never replace a new connection's view"
        );
    }
}
