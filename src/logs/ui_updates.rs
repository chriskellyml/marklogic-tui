//! Scheduling for UI work, independent of live collection's polling cadence.
use super::*;

pub(crate) struct SidebarStatus {
    checked: Instant,
    day: NaiveDate,
    pub freshness: BTreeMap<String, CacheFreshness>,
    pub summaries: BTreeMap<String, (usize, u64, String)>,
    pub modes: BTreeMap<String, TrackingMode>,
}

type PortRows = Vec<(String, [Option<usize>; 3])>;

#[derive(PartialEq, Eq)]
struct GridKey {
    day: NaiveDate,
    filter: String,
    ports: BTreeMap<String, PortFilter>,
    included_only: bool,
    file_count: usize,
}

#[derive(Default)]
pub(super) struct SidebarRows {
    key: Option<GridKey>,
    days: BTreeMap<u32, PortRows>,
}

impl SidebarRows {
    pub(super) fn clear(&mut self) {
        self.days.clear();
    }
}

impl LogViewer {
    pub(crate) fn ui_updates_pending(&self) -> bool {
        self.busy
            || self.cache_load.is_some()
            || !self.parse_jobs.is_empty()
            || self.preview_pending()
            || self.index_filter_changed.is_some()
            || self.search_changed_at.is_some()
            || self.filter_dialog().is_some()
            || self.activity_graph.pending()
            || self.combined_dirty
    }

    /// Move the file cursor now; only build its preview once browsing settles.
    pub(crate) fn defer_file_preview(&mut self) {
        self.ensure_cursor_time();
        self.preview_changed = Some(Instant::now());
    }

    pub(crate) fn preview_pending(&self) -> bool {
        self.preview_changed.is_some()
    }

    /// Enter, Tab, and clicking content explicitly open the final selection.
    pub(crate) fn flush_file_preview(&mut self) {
        if self.preview_changed.take().is_none() {
            return;
        }
        self.rebuild_lines();
        if let Some(time) = self.cursor_time {
            self.seek_nearest(time);
        } else {
            self.scroll_to_bottom();
        }
    }

    pub(crate) fn flush_search(&mut self) {
        if self.search_changed_at.take().is_some() {
            self.filter();
            self.scroll_to_bottom();
        }
        if self.index_filter_changed.is_some() {
            self.index_filter_changed = Some(Instant::now() - PREVIEW_DELAY);
            self.tick_index_filter();
        }
    }

    pub(super) fn invalidate_sidebar_status(&self) {
        self.sidebar_status.borrow_mut().take();
        self.sidebar_rows.borrow_mut().clear();
    }

    pub(super) fn cached_port_rows(&self, day: u32) -> PortRows {
        let key = GridKey {
            day: self.now().date(),
            filter: self.file_filter.clone(),
            ports: self.port_filters.clone(),
            included_only: self.include_only_marked_ports,
            file_count: self.files.len(),
        };
        let mut cached = self.sidebar_rows.borrow_mut();
        if cached.key.as_ref() != Some(&key) {
            cached.key = Some(key);
            cached.clear();
        }
        if let Some(rows) = cached.days.get(&day) {
            return rows.clone();
        }
        let rows = self.build_port_rows_for_day(day);
        // Calendar navigation must not retain a growing cache of every day visited.
        if cached.days.len() >= 32 {
            cached.clear();
        }
        cached.days.insert(day, rows.clone());
        rows
    }

    /// Freshness and age labels don't need a catalog scan on every frame.
    /// Downloads, discovery, and tracking changes invalidate this immediately.
    pub(crate) fn sidebar_status(&self, now: DateTime<Utc>) -> Arc<SidebarStatus> {
        let mut cached = self.sidebar_status.borrow_mut();
        if let Some(status) = cached.as_ref()
            && status.checked.elapsed() < STATUS_INTERVAL
            && status.day == self.now().date()
        {
            return status.clone();
        }
        let status = Arc::new(SidebarStatus {
            checked: Instant::now(),
            day: self.now().date(),
            freshness: self
                .grid_freshness(now)
                .into_iter()
                .map(|(name, state)| (name.to_owned(), state))
                .collect(),
            summaries: self
                .group_summaries(now)
                .into_iter()
                .map(|(name, summary)| (name.to_owned(), summary))
                .collect(),
            modes: self
                .grid_tracking_modes()
                .into_iter()
                .map(|(name, mode)| (name.to_owned(), mode))
                .collect(),
        });
        *cached = Some(status.clone());
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> LogViewer {
        let mut logs = LogViewer::default();
        logs.files.push(LogFile {
            host: "one".into(),
            path: "/Logs/ErrorLog.txt".into(),
            name: "ErrorLog.txt".into(),
            size: 20,
            modified: Utc::now().to_rfc3339(),
            present: true,
        });
        logs
    }

    #[test]
    fn sidebar_status_reuses_results_until_sixty_seconds_or_invalidation() {
        let mut logs = catalog();
        let now = Utc::now();
        let first = logs.sidebar_status(now);
        assert!(Arc::ptr_eq(
            &first,
            &logs.sidebar_status(now + Days::seconds(30))
        ));
        assert_eq!(first.freshness["ErrorLog.txt"], CacheFreshness::Missing);
        drop(first);
        let checked = {
            let mut cached = logs.sidebar_status.borrow_mut();
            let status = Arc::get_mut(cached.as_mut().unwrap()).unwrap();
            status.checked = Instant::now() - STATUS_INTERVAL;
            status.checked
        };
        assert!(logs.sidebar_status(now).checked > checked);
        let before = logs.sidebar_status(now);
        logs.files[0].size = 0;
        logs.sort_files(); // Inventory changes invalidate both status and grid indexes.
        let after = logs.sidebar_status(now);
        assert!(!Arc::ptr_eq(&before, &after));
        assert_eq!(after.freshness["ErrorLog.txt"], CacheFreshness::NoData);
    }

    #[test]
    fn cached_grid_tracks_filters_and_catalog_changes() {
        let mut logs = catalog();
        assert_eq!(logs.visible_port_rows().len(), 1);
        logs.file_filter = "unmatched".into();
        assert!(logs.visible_port_rows().is_empty());
        logs.file_filter.clear();
        assert_eq!(logs.visible_port_rows().len(), 1);
        logs.port_filters.insert("----".into(), PortFilter::Exclude);
        assert!(logs.visible_port_rows().is_empty());
        logs.port_filters.clear();
        assert_eq!(logs.visible_port_rows().len(), 1);
        logs.files.clear();
        assert!(logs.visible_port_rows().is_empty());
    }
}
