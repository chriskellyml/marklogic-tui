//! Foreground downloads take priority over automatic collection and cache maintenance.
use super::*;

enum Target {
    File(String),
    Merged(String, u32),
    OnDemand,
    Discovery,
}

struct Entry {
    label: String,
    target: Option<Target>,
    queued: bool,
}

impl LogViewer {
    pub(super) fn foreground_collection(&self) -> bool {
        self.activity.as_ref().is_some_and(|activity| {
            matches!(
                activity.label,
                "Downloading logs" | "Downloading merged logs" | "Refreshing logs"
            )
        })
    }

    /// Stop the current worker on its own channel, retaining explicit requests.
    /// Late results from the stopped worker must never finish the replacement job.
    fn pause_collection(&mut self, skip: Option<&str>) {
        let mut manual = std::mem::take(&mut self.manual_downloads_queued);
        if self.foreground_collection()
            && let Some(activity) = &self.activity
        {
            manual.extend(
                activity
                    .jobs
                    .iter()
                    .filter(|(_, job)| !job.done)
                    .map(|((_, file), _)| file.clone()),
            );
        }
        let forced = std::mem::take(&mut self.forced_downloads_queued);
        let merged = std::mem::take(&mut self.merged_downloads_queued);
        let demand = std::mem::take(&mut self.demand_files_queued);
        let demand_all = self.demand_queued;
        let refresh = self.refresh_queued;
        self.cancel();
        self.manual_downloads_queued = manual;
        self.forced_downloads_queued = forced;
        self.merged_downloads_queued = merged;
        self.demand_files_queued = demand;
        self.demand_queued = demand_all;
        self.refresh_queued = refresh;
        if let Some(name) = skip {
            self.manual_downloads_queued.remove(name);
            self.forced_downloads_queued.remove(name);
            self.demand_files_queued.remove(name);
        }
        self.last_poll = Instant::now();
    }

    pub fn download_now(&mut self, rt: &Runtime) {
        let Some(file) = self.selected_file().cloned() else {
            self.message = "No log is available in this cell.".into();
            return;
        };
        self.start_file_now(rt, &file.name);
    }

    pub(super) fn start_file_now(&mut self, rt: &Runtime, name: &str) {
        if !self.has_downloadable_file(name) {
            self.message = "No non-empty remote log is available to download.".into();
            return;
        }
        if self.storage_busy {
            self.message = "Log storage is busy; retry shortly.".into();
            return;
        }
        if self.endpoints.is_empty() {
            self.manual_downloads_queued.insert(name.into());
            self.message =
                format!("Waiting for host discovery: {name}. Q: queue; Ctrl+D: retry discovery.");
            if self.busy {
                self.pause_collection(None);
            }
            if self.client.is_some() {
                self.refresh(rt);
            } else {
                self.message =
                    format!("Cannot download {name}: no connected host. Reconnect and retry.");
            }
            return;
        }
        if self.busy {
            self.pause_collection(Some(name));
        }
        self.manual_downloads_queued.remove(name);
        self.forced_downloads_queued.remove(name);
        self.demand_files_queued.remove(name);
        self.start_full_download(rt, name);
    }

    /// Called before automatic work, including immediately after discovery.
    pub(super) fn start_queued_downloads(&mut self, rt: &Runtime) {
        if !self.automatic_collection_allowed() {
            return;
        }
        while !self.busy && !self.endpoints.is_empty() {
            if let Some(name) = self.manual_downloads_queued.pop_first() {
                self.start_full_download(rt, &name);
            } else if let Some(name) = self.forced_downloads_queued.pop_first() {
                if self.files.iter().any(|file| {
                    file.name == name
                        && matches!(
                            self.tracking_mode(file),
                            TrackingMode::OnDemand | TrackingMode::Live
                        )
                }) {
                    self.start_full_download(rt, &name);
                }
            } else if let Some((port, day)) = self.merged_downloads_queued.pop_first() {
                self.start_merged_download(rt, &port, day);
            } else if self.demand_queued {
                self.demand_queued = false;
                self.demand_files_queued.clear();
                self.sync_on_demand(rt, None);
            } else if let Some(name) = self.demand_files_queued.pop_first() {
                self.sync_on_demand(rt, Some(name));
            } else {
                break;
            }
        }
    }

    pub fn download_progress(&self) -> Option<(u16, String)> {
        let activity = self.activity.as_ref().filter(|_| self.busy)?;
        let percent = if activity.download {
            u16::from(activity.download_percentage())
        } else if let Some(total) = activity.work_bytes.filter(|total| *total > 0) {
            ((activity.received.load(Ordering::Relaxed) as f64 / total as f64) * 100.0).min(99.0)
                as u16
        } else if activity.total > 0 {
            (activity.completed.min(activity.total) * 100 / activity.total) as u16
        } else {
            0
        };
        let queue = self.manual_downloads_queued.len()
            + self.forced_downloads_queued.len()
            + self.merged_downloads_queued.len()
            + self.demand_files_queued.len()
            + usize::from(self.demand_queued);
        let bytes = format_bytes(activity.received.load(Ordering::Relaxed));
        Some((
            percent,
            format!(
                "~{percent}% · {}/{} done · {bytes} · {queue} queued · Q: queue · ^D: start · {}",
                activity.completed, activity.total, activity.label
            ),
        ))
    }

    fn download_entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        if let Some(activity) = self.activity.as_ref().filter(|_| self.busy) {
            if activity.jobs.is_empty() {
                entries.push(Entry {
                    label: format!(
                        "RUNNING {} · {}s",
                        activity.label,
                        activity.started.elapsed().as_secs()
                    ),
                    target: None,
                    queued: false,
                });
                for (host, file) in &activity.active {
                    entries.push(Entry {
                        label: format!("WORKING {}:{file}", self.host_alias(host)),
                        target: None,
                        queued: false,
                    });
                }
            }
            for ((host, file), job) in &activity.jobs {
                let key = (host.clone(), file.clone());
                let phase = if job.done {
                    if self.download_results.get(&key) == Some(&false) {
                        "FAILED"
                    } else {
                        "DONE"
                    }
                } else if activity.staging.contains(&key) {
                    "SAVING"
                } else if activity.active.contains(&key) {
                    if job.received.load(Ordering::Relaxed) == 0 {
                        "WAITING FOR SERVER"
                    } else {
                        "DOWNLOADING"
                    }
                } else {
                    "WAITING FOR SLOT"
                };
                let response_size = job.response_size.load(Ordering::Relaxed);
                let total = if response_size > 0 {
                    Some(response_size)
                } else {
                    job.size
                };
                let total = total.map(format_bytes).unwrap_or_else(|| "unknown".into());
                entries.push(Entry {
                    label: format!(
                        "{phase} {}:{file} · {} / {total} received",
                        self.host_alias(host),
                        format_bytes(job.received.load(Ordering::Relaxed))
                    ),
                    target: Some(Target::File(file.clone())),
                    queued: false,
                });
            }
        }
        let waiting = self
            .activity
            .as_ref()
            .filter(|_| self.busy)
            .map(|activity| format!(" · after {}", activity.label))
            .unwrap_or_default();
        let names: BTreeSet<_> = self
            .manual_downloads_queued
            .iter()
            .chain(&self.forced_downloads_queued)
            .chain(&self.demand_files_queued)
            .collect();
        for name in names {
            entries.push(Entry {
                label: format!("QUEUED {name}{waiting}"),
                target: Some(Target::File(name.clone())),
                queued: true,
            });
        }
        for (port, day) in &self.merged_downloads_queued {
            entries.push(Entry {
                label: format!("QUEUED port {port} Err + Acc · {day} days ago{waiting}"),
                target: Some(Target::Merged(port.clone(), *day)),
                queued: true,
            });
        }
        if self.demand_queued {
            entries.push(Entry {
                label: format!("QUEUED on-demand refresh{waiting}"),
                target: Some(Target::OnDemand),
                queued: true,
            });
        }
        if self.refresh_queued {
            entries.push(Entry {
                label: format!("QUEUED host discovery{waiting}"),
                target: Some(Target::Discovery),
                queued: true,
            });
        }
        for ((host, file), successful) in &self.download_results {
            if !successful
                && !self
                    .activity
                    .as_ref()
                    .is_some_and(|a| a.jobs.contains_key(&(host.clone(), file.clone())))
            {
                entries.push(Entry {
                    label: format!("FAILED {}:{file} · Enter: retry", self.host_alias(host)),
                    target: Some(Target::File(file.clone())),
                    queued: false,
                });
            }
        }
        entries
    }

    pub fn download_queue_rows(&self) -> Vec<String> {
        self.download_entries()
            .into_iter()
            .map(|entry| entry.label)
            .collect()
    }

    pub fn move_download_queue(&mut self, forward: bool) {
        let last = self.download_entries().len().saturating_sub(1);
        self.download_queue_selected = if forward {
            self.download_queue_selected.saturating_add(1).min(last)
        } else {
            self.download_queue_selected.saturating_sub(1).min(last)
        };
    }

    pub fn kick_download_queue(&mut self, rt: &Runtime) {
        let entries = self.download_entries();
        let index = self
            .download_queue_selected
            .min(entries.len().saturating_sub(1));
        let Some(entry) = entries.into_iter().nth(index) else {
            return;
        };
        match entry.target {
            Some(Target::File(name)) => self.start_file_now(rt, &name),
            Some(Target::Merged(port, day)) => {
                self.pause_collection(None);
                self.merged_downloads_queued.remove(&(port.clone(), day));
                self.start_merged_download(rt, &port, day);
            }
            Some(Target::OnDemand) => {
                self.pause_collection(None);
                self.demand_queued = false;
                self.sync_on_demand(rt, None);
            }
            Some(Target::Discovery) => {
                self.pause_collection(None);
                self.refresh_queued = false;
                self.refresh(rt);
            }
            None => self.message = "Select a queued file and press Enter to start it now; Ctrl+D starts the file selected in Files.".into(),
        }
    }

    pub fn remove_download_queue_entry(&mut self) {
        let entries = self.download_entries();
        let index = self
            .download_queue_selected
            .min(entries.len().saturating_sub(1));
        let Some(entry) = entries.into_iter().nth(index) else {
            return;
        };
        if !entry.queued {
            self.message =
                "This entry is running or finished. x outside Downloads cancels current work."
                    .into();
            return;
        }
        match entry.target {
            Some(Target::File(name)) => {
                self.manual_downloads_queued.remove(&name);
                self.forced_downloads_queued.remove(&name);
                self.demand_files_queued.remove(&name);
            }
            Some(Target::Merged(port, day)) => {
                self.merged_downloads_queued.remove(&(port, day));
            }
            Some(Target::OnDemand) => self.demand_queued = false,
            Some(Target::Discovery) => self.refresh_queued = false,
            None => return,
        }
        self.download_queue_selected = index.min(self.download_entries().len().saturating_sub(1));
        self.message = "Removed queued download.".into();
    }
}
