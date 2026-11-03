//! Durable knowledge about closed history and successful host inventories.
use super::*;

const STATE_FILE: &str = "history-state.cfg";
pub(super) const RETAINED_FILE: &str = "retained-history.cfg";

pub(super) fn archive_identity(name: &str) -> String {
    name.split_once("-ref-")
        .and_then(|(_, tail)| tail.split('-').next())
        .filter(|id| id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .unwrap_or_else(|| hash_key(name))
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct RetainedCatalog {
    pub offset: i32,
    pub entries: BTreeMap<String, RetainedEntry>,
}

#[derive(Serialize, Deserialize)]
pub(super) struct RetainedEntry {
    pub snapshot: Snapshot,
    pub days: BTreeSet<NaiveDate>,
}

pub(super) fn same_day(left: &LogFile, right: &LogFile, offset: FixedOffset) -> bool {
    DateTime::parse_from_rfc3339(&left.modified)
        .ok()
        .zip(DateTime::parse_from_rfc3339(&right.modified).ok())
        .is_some_and(|(left, right)| {
            left.with_timezone(&offset).date_naive() == right.with_timezone(&offset).date_naive()
        })
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct HistoryState {
    server_offset: Option<i32>,
    completed_day: Option<NaiveDate>,
    pub inventories: BTreeMap<String, InventoryReceipt>,
    missing: BTreeSet<MissingLog>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct InventoryReceipt {
    day: NaiveDate,
    tracked: BTreeSet<String>,
    paths: BTreeSet<String>,
}

impl InventoryReceipt {
    pub fn covers(&self, day: NaiveDate, tracked: &BTreeSet<String>) -> bool {
        self.day == day && self.tracked.is_superset(tracked)
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
struct MissingLog {
    host: String,
    stream: String,
    day: NaiveDate,
}

impl Snapshot {
    /// A partial tail fetched later is still partial. Only a closed physical
    /// rotation, fully downloaded after its modification day, can be settled.
    pub(super) fn settled(&self, offset: FixedOffset) -> bool {
        LogViewer::file_days_ago(&self.file).is_some_and(|age| age > 0)
            && self.full_size == Some(self.text_bytes)
            && self.file.size == self.text_bytes
            && self.start == 0
            && self.end == self.length
            && DateTime::parse_from_rfc3339(&self.file.modified)
                .ok()
                .zip(
                    self.updated_at
                        .as_deref()
                        .and_then(|time| DateTime::parse_from_rfc3339(time).ok()),
                )
                .is_some_and(|(modified, downloaded)| {
                    downloaded.with_timezone(&offset).date_naive()
                        > modified.with_timezone(&offset).date_naive()
                })
    }
}

impl LogViewer {
    pub(super) fn load_history_state(&mut self) {
        self.history_state = fs::read(self.cache_root.join(STATE_FILE))
            .ok()
            .and_then(|data| serde_json::from_slice(&data).ok())
            .unwrap_or_default();
        self.history_checked = self.history_state.completed_day;
        if self.clock.is_none()
            && let Some(offset) = self
                .history_state
                .server_offset
                .and_then(FixedOffset::east_opt)
        {
            self.server_offset = offset;
        }
    }

    fn save_history_state(&mut self) {
        self.history_state.server_offset = Some(self.server_offset.local_minus_utc());
        let result = fs::create_dir_all(&self.cache_root)
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                atomic_write(
                    &self.cache_root.join(STATE_FILE),
                    &serde_json::to_vec(&self.history_state)?,
                )
            });
        if let Err(error) = result {
            self.message = format!("Saving historical log state: {error:#}");
        }
    }

    pub(super) fn record_history_inventory(&mut self, host: &str) {
        let today = self.now().date();
        if !self.history_inventory_known(host) && self.history_checked == Some(today) {
            self.history_checked = None;
            self.history_state.completed_day = None;
        }
        for file in self.files.iter().filter(|file| {
            file.host == host && !self.retained_archive_days.contains_key(&file.key())
        }) {
            let Some(age) = Self::file_days_ago(file).filter(|age| *age > 0) else {
                continue;
            };
            let day = DateTime::parse_from_rfc3339(&file.modified)
                .ok()
                .map(|time| time.with_timezone(&self.server_offset).date_naive())
                .unwrap_or(today - Days::days(i64::from(age)));
            let missing = MissingLog {
                host: host.to_owned(),
                stream: Self::stream_name(&file.name),
                day,
            };
            if file.present {
                self.history_state.missing.remove(&missing);
            } else {
                self.history_state.missing.insert(missing);
            }
        }
        self.history_state.inventories.insert(
            host.to_owned(),
            InventoryReceipt {
                day: today,
                tracked: self.tracked_names(),
                paths: self
                    .files
                    .iter()
                    .filter(|file| file.host == host && file.present)
                    .map(|file| file.path.clone())
                    .collect(),
            },
        );
        self.save_history_state();
    }

    pub(super) fn history_inventory_known(&self, host: &str) -> bool {
        self.history_state
            .inventories
            .get(host)
            .is_some_and(|receipt| receipt.covers(self.now().date(), &self.tracked_names()))
    }

    pub(super) fn historical_path_known_present(&self, file: &LogFile) -> bool {
        if self.history_missing(file) {
            return false;
        }
        self.history_state
            .inventories
            .get(&file.host)
            .is_some_and(|receipt| {
                receipt.day == self.now().date() && receipt.paths.contains(&file.path)
            })
    }

    pub(super) fn history_missing(&self, file: &LogFile) -> bool {
        let Some(age) = Self::file_days_ago(file).filter(|age| *age > 0) else {
            return false;
        };
        let day = DateTime::parse_from_rfc3339(&file.modified)
            .ok()
            .map(|time| time.with_timezone(&self.server_offset).date_naive())
            .unwrap_or(self.now().date() - Days::days(i64::from(age)));
        self.history_state.missing.contains(&MissingLog {
            host: file.host.clone(),
            stream: Self::stream_name(&file.name),
            day,
        })
    }

    pub(super) fn mark_history_checked(&mut self, day: NaiveDate) {
        self.history_checked = Some(day);
        self.history_state.completed_day = Some(day);
        self.save_history_state();
    }
}
