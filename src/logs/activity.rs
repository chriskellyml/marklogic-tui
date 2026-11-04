//! Cached activity histogram. Drawing only polls a channel and reads two strings;
//! scanning timestamps and scaling bars happen on a single, lazy worker.
use chrono::{Datelike, NaiveDateTime};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
    mpsc::{self, Receiver, Sender},
};
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(crate) struct Chart {
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub bars: [String; 2],
    pub bands: Vec<bool>,
    pub series_bars: Vec<String>,
    pub interval: &'static str,
}

struct Selection {
    chart: Chart,
    first: usize,
    last: Option<usize>,
    apply_at: Option<Instant>,
}

const SELECTION_PREVIEW: Duration = Duration::from_millis(700);

#[derive(Clone, Copy)]
enum BandInterval {
    Seconds(i64),
    Months(i32),
}

impl BandInterval {
    fn phase(self, time: NaiveDateTime) -> bool {
        let index = match self {
            Self::Seconds(seconds) => {
                // Unix time begins on a Thursday; shift weeks to start Monday.
                let offset = if seconds == 7 * 86_400 { 3 * 86_400 } else { 0 };
                (time.and_utc().timestamp() + offset).div_euclid(seconds)
            }
            Self::Months(months) => {
                let month = i64::from(time.year()) * 12 + i64::from(time.month0());
                month.div_euclid(i64::from(months))
            }
        };
        index.rem_euclid(2) != 0
    }
}

fn band_interval(duration_nanos: i128, width: usize) -> (BandInterval, &'static str) {
    // Aim for at least six columns per band, without painting one colour per
    // character when the time range is much wider than the terminal.
    let target = duration_nanos * 6 / width.max(1) as i128;
    let choices: [(i64, BandInterval, &str); 20] = [
        (60, BandInterval::Seconds(60), "1 min"),
        (300, BandInterval::Seconds(300), "5 min"),
        (900, BandInterval::Seconds(900), "15 min"),
        (1800, BandInterval::Seconds(1800), "30 min"),
        (3600, BandInterval::Seconds(3600), "1 hour"),
        (10800, BandInterval::Seconds(10800), "3 hours"),
        (21600, BandInterval::Seconds(21600), "6 hours"),
        (43200, BandInterval::Seconds(43200), "12 hours"),
        (86400, BandInterval::Seconds(86400), "1 day"),
        (604800, BandInterval::Seconds(604800), "1 week"),
        (2629800, BandInterval::Months(1), "1 month"),
        (7889400, BandInterval::Months(3), "3 months"),
        (15778800, BandInterval::Months(6), "6 months"),
        (31557600, BandInterval::Months(12), "1 year"),
        (157788000, BandInterval::Months(60), "5 years"),
        (315576000, BandInterval::Months(120), "10 years"),
        (3155760000, BandInterval::Months(1200), "100 years"),
        (31557600000, BandInterval::Months(12000), "1000 years"),
        (315576000000, BandInterval::Months(120000), "10000 years"),
        (3155760000000, BandInterval::Months(1200000), "100000 years"),
    ];
    let (_, interval, label) = choices
        .iter()
        .find(|(seconds, _, _)| i128::from(*seconds) * 1_000_000_000 >= target)
        .unwrap_or_else(|| choices.last().unwrap());
    (*interval, *label)
}

struct Request {
    revision: u64,
    times: Arc<Vec<NaiveDateTime>>,
    width: usize,
    series: Arc<Vec<Vec<NaiveDateTime>>>,
}

struct Worker {
    tx: Sender<Request>,
    rx: Receiver<(u64, Chart)>,
}

#[derive(Default)]
pub(crate) struct ActivityGraph {
    times: Arc<Vec<NaiveDateTime>>,
    series: Arc<Vec<Vec<NaiveDateTime>>>,
    width: usize,
    revision: Arc<AtomicU64>,
    worker: Option<Worker>,
    pending: bool,
    pub chart: Option<Chart>,
    selection: Option<Selection>,
}

impl ActivityGraph {
    /// Called only when the view's records/filters are rebuilt, never on scroll.
    pub fn set_data(&mut self, times: Vec<NaiveDateTime>) {
        if self.times.as_slice() == times.as_slice() && self.series.is_empty() {
            return;
        }
        self.times = Arc::new(times);
        self.series = Arc::default();
        self.request();
    }

    pub fn set_series(&mut self, times: Vec<NaiveDateTime>, series: Vec<Vec<NaiveDateTime>>) {
        if self.times.as_slice() == times.as_slice() && self.series.as_slice() == series.as_slice()
        {
            return;
        }
        self.times = Arc::new(times);
        self.series = Arc::new(series);
        self.request();
    }

    fn request(&mut self) {
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        self.chart = None;
        self.pending = !self.times.is_empty() && self.width > 0;
        if !self.pending {
            return;
        }
        let worker = self.worker.get_or_insert_with(|| {
            let (tx, requests) = mpsc::channel::<Request>();
            let (results, rx) = mpsc::channel();
            let current = self.revision.clone();
            std::thread::spawn(move || {
                while let Ok(mut request) = requests.recv() {
                    // Coalesce rapid filter changes and resizes before doing work.
                    while let Ok(newer) = requests.try_recv() {
                        request = newer;
                    }
                    let revision = request.revision;
                    if let Some(chart) = calculate(&request, &current)
                        && results.send((revision, chart)).is_err()
                    {
                        break;
                    }
                }
            });
            Worker { tx, rx }
        });
        if worker
            .tx
            .send(Request {
                revision,
                times: self.times.clone(),
                width: self.width,
                series: self.series.clone(),
            })
            .is_err()
        {
            self.pending = false;
        }
    }

    pub fn poll(&mut self, width: usize) {
        if self.width != width {
            self.selection = None;
            self.width = width;
            self.request();
        }
        if let Some(worker) = &self.worker {
            while let Ok((revision, chart)) = worker.rx.try_recv() {
                if revision == self.revision.load(Ordering::Relaxed) {
                    self.chart = Some(chart);
                    self.pending = false;
                }
            }
        }
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Hold the original axis steady until the selection is applied or cancelled.
    pub fn displayed_chart(&self) -> Option<&Chart> {
        self.selection
            .as_ref()
            .map(|s| &s.chart)
            .or(self.chart.as_ref())
    }

    pub fn selection_columns(&self) -> Option<(usize, usize)> {
        let selection = self.selection.as_ref()?;
        let last = selection.last.unwrap_or(selection.first);
        Some((selection.first.min(last), selection.first.max(last)))
    }

    pub fn click(&mut self, column: usize) -> bool {
        if let Some(selection) = self.selection.as_mut()
            && selection.last.is_none()
        {
            selection.last = Some(column.min(selection.chart.bands.len().saturating_sub(1)));
            selection.apply_at = Some(Instant::now() + SELECTION_PREVIEW);
            return true;
        }
        if let Some(chart) = self
            .displayed_chart()
            .filter(|chart| !chart.bands.is_empty())
            .cloned()
        {
            self.selection = Some(Selection {
                first: column.min(chart.bands.len() - 1),
                chart,
                last: None,
                apply_at: None,
            });
        }
        false
    }

    pub fn cancel_selection(&mut self) {
        self.selection = None;
    }

    #[cfg(test)]
    pub(super) fn finish_preview_for_test(&mut self) {
        if let Some(selection) = self.selection.as_mut() {
            selection.apply_at = Some(Instant::now());
        }
    }

    pub fn take_ready_window(&mut self) -> Option<(NaiveDateTime, NaiveDateTime)> {
        let selection = self.selection.as_ref()?;
        if selection
            .apply_at
            .is_none_or(|deadline| Instant::now() < deadline)
        {
            return None;
        }
        let selection = self.selection.take()?;
        let (left, right) = (
            selection.first.min(selection.last?),
            selection.first.max(selection.last?),
        );
        let width = selection.chart.bands.len() as i128;
        let nanos = |time: NaiveDateTime| {
            let time = time.and_utc();
            i128::from(time.timestamp()) * 1_000_000_000 + i128::from(time.timestamp_subsec_nanos())
        };
        let first = nanos(selection.chart.start);
        let duration = nanos(selection.chart.end) - first + 1;
        // Match the inclusive buckets used by calculate(), including the last instant.
        let ceil_div = |n: i128| (n + width - 1) / width;
        let start = (first + ceil_div(duration * left as i128)).min(nanos(selection.chart.end));
        let end = (first + ceil_div(duration * (right as i128 + 1)) - 1).max(start);
        let to_time = |n: i128| {
            chrono::DateTime::from_timestamp(
                n.div_euclid(1_000_000_000) as i64,
                n.rem_euclid(1_000_000_000) as u32,
            )
            .expect("a point within the chart range is a valid timestamp")
            .naive_utc()
        };
        Some((to_time(start), to_time(end)))
    }
}

impl Drop for ActivityGraph {
    fn drop(&mut self) {
        // Stop an in-flight scan promptly; dropping the sender then ends the worker.
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}

fn calculate(request: &Request, current: &AtomicU64) -> Option<Chart> {
    let mut start = *request.times.first()?;
    let mut end = start;
    for batch in request.times.chunks(4096) {
        if current.load(Ordering::Relaxed) != request.revision {
            return None;
        }
        for &time in batch {
            start = start.min(time);
            end = end.max(time);
        }
    }
    // Equal-duration buckets cover the inclusive range, including its last instant.
    // Nanoseconds in i128 preserve subsecond bursts without duration overflow.
    let nanos = |time: NaiveDateTime| {
        let utc = time.and_utc();
        i128::from(utc.timestamp()) * 1_000_000_000 + i128::from(utc.timestamp_subsec_nanos())
    };
    let first = nanos(start);
    let duration = nanos(end) - first + 1;
    let mut buckets = vec![0u64; request.width];
    for batch in request.times.chunks(4096) {
        if current.load(Ordering::Relaxed) != request.revision {
            return None;
        }
        for &time in batch {
            let bucket = ((nanos(time) - first) * request.width as i128 / duration) as usize;
            buckets[bucket.min(request.width - 1)] += 1;
        }
    }
    let maximum = *buckets.iter().max()?;
    let mut bars = [String::new(), String::new()];
    const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    for count in buckets {
        // Sixteen levels across two cells; nonempty buckets stay visible.
        let level = if count == 0 {
            0
        } else {
            ((u128::from(count) * 16 + u128::from(maximum) / 2) / u128::from(maximum)).max(1)
                as usize
        };
        bars[0].push(BLOCKS[level.saturating_sub(8)]);
        bars[1].push(BLOCKS[level.min(8)]);
    }
    let mut series_bars = Vec::with_capacity(request.series.len());
    for series in request.series.iter() {
        let mut counts = vec![0u64; request.width];
        for batch in series.chunks(4096) {
            if current.load(Ordering::Relaxed) != request.revision {
                return None;
            }
            for &time in batch {
                let bucket = ((nanos(time) - first) * request.width as i128 / duration) as usize;
                counts[bucket.min(request.width - 1)] += 1;
            }
        }
        let maximum = counts.iter().copied().max().unwrap_or(0);
        series_bars.push(
            counts
                .into_iter()
                .map(|count| {
                    let level = if count == 0 {
                        0
                    } else {
                        ((u128::from(count) * 8 + u128::from(maximum) / 2) / u128::from(maximum))
                            .max(1) as usize
                    };
                    BLOCKS[level]
                })
                .collect(),
        );
    }
    let (interval, label) = band_interval(duration - 1, request.width);
    let bands = (0..request.width)
        .map(|column| {
            // Sample each cell's midpoint; the grid is approximate by nature.
            let instant = first + duration * (2 * column as i128 + 1) / (2 * request.width as i128);
            let seconds = instant.div_euclid(1_000_000_000) as i64;
            let nanos = instant.rem_euclid(1_000_000_000) as u32;
            let time = chrono::DateTime::from_timestamp(seconds, nanos)
                .expect("a point within the log range is a valid timestamp")
                .naive_utc();
            interval.phase(time)
        })
        .collect();
    Some(Chart {
        start,
        end,
        bars,
        bands,
        series_bars,
        interval: label,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::{HostView, LogViewer, RowSource, ViewRow};
    use ratatui::text::Line;
    use std::time::Duration;

    fn time(seconds: i64) -> NaiveDateTime {
        chrono::DateTime::from_timestamp(seconds, 0)
            .unwrap()
            .naive_utc()
    }

    fn chart(times: Vec<NaiveDateTime>, width: usize) -> Chart {
        calculate(
            &Request {
                revision: 0,
                times: Arc::new(times),
                width,
                series: Arc::default(),
            },
            &AtomicU64::new(0),
        )
        .unwrap()
    }

    #[test]
    fn individual_series_share_the_range_and_scale_independently() {
        let result = calculate(
            &Request {
                revision: 0,
                times: Arc::new(vec![time(0), time(0), time(2), time(4)]),
                series: Arc::new(vec![vec![time(0), time(0), time(2)], vec![time(4)], vec![]]),
                width: 5,
            },
            &AtomicU64::new(0),
        )
        .unwrap();
        assert_eq!(result.series_bars, ["█ ▄  ", "    █", "     "]);
        assert_eq!((result.start, result.end), (time(0), time(4)));
    }

    #[test]
    fn bars_scale_to_two_rows_and_leave_gaps_empty() {
        let mut times = vec![time(0); 8];
        times.extend(vec![time(2); 4]);
        times.push(time(4));
        let chart = chart(times, 5);
        assert_eq!(chart.start, time(0));
        assert_eq!(chart.end, time(4));
        assert_eq!(chart.bars, ["█    ", "█ █ ▂"]);
    }

    #[test]
    fn range_uses_actual_extremes_and_preserves_subsecond_activity() {
        let start = time(0);
        let end = start + chrono::Duration::nanoseconds(4);
        let chart = chart(
            vec![end, start, start + chrono::Duration::nanoseconds(2)],
            5,
        );
        assert_eq!((chart.start, chart.end), (start, end));
        assert_eq!(chart.bars, ["█ █ █", "█ █ █"]);
    }

    #[test]
    fn single_instant_and_single_column_are_supported() {
        assert_eq!(chart(vec![time(5); 3], 4).bars, ["█   ", "█   "]);
        assert_eq!(chart(vec![time(0), time(5)], 1).bars, ["█", "█"]);
    }

    #[test]
    fn selection_uses_inclusive_bucket_boundaries_and_freezes_the_axis() {
        let mut graph = ActivityGraph::default();
        graph.chart = Some(chart(vec![time(0), time(4)], 5));
        assert!(!graph.click(3));
        assert!(graph.click(1));
        assert_eq!(graph.selection_columns(), Some((1, 3)));
        graph.set_data(vec![time(0), time(20)]);
        assert_eq!(graph.displayed_chart().unwrap().end, time(4));
        assert!(graph.take_ready_window().is_none());
        graph.finish_preview_for_test();
        assert_eq!(
            graph.take_ready_window(),
            Some((
                time(0) + chrono::Duration::nanoseconds(800_000_001),
                time(0) + chrono::Duration::nanoseconds(3_200_000_000),
            )),
        );
        graph.chart = Some(chart(vec![time(5)], 4));
        graph.click(2);
        graph.click(2);
        graph.finish_preview_for_test();
        assert_eq!(graph.take_ready_window(), Some((time(5), time(5))));
    }

    #[test]
    fn time_bands_adapt_to_width_and_follow_calendar_boundaries() {
        let seconds = |n: i128| n * 1_000_000_000;
        assert_eq!(band_interval(seconds(600), 60).1, "1 min");
        assert_eq!(band_interval(seconds(6 * 3600), 36).1, "1 hour");
        assert_eq!(band_interval(seconds(8 * 7 * 86400), 48).1, "1 week");
        assert_eq!(band_interval(seconds(360 * 86400), 72).1, "1 month");
        let date = |year, month, day, hour, minute| {
            chrono::NaiveDate::from_ymd_opt(year, month, day)
                .unwrap()
                .and_hms_opt(hour, minute, 0)
                .unwrap()
        };
        assert_ne!(
            BandInterval::Seconds(3600).phase(date(2026, 1, 1, 0, 59)),
            BandInterval::Seconds(3600).phase(date(2026, 1, 1, 1, 0))
        );
        assert_ne!(
            BandInterval::Seconds(604800).phase(date(2026, 1, 4, 23, 59)),
            BandInterval::Seconds(604800).phase(date(2026, 1, 5, 0, 0))
        );
        assert_ne!(
            BandInterval::Months(1).phase(date(2026, 1, 31, 23, 59)),
            BandInterval::Months(1).phase(date(2026, 2, 1, 0, 0))
        );
        assert_ne!(
            BandInterval::Months(12).phase(date(2026, 12, 31, 23, 59)),
            BandInterval::Months(12).phase(date(2027, 1, 1, 0, 0))
        );
    }

    #[test]
    fn tiny_nonzero_buckets_remain_visible_and_cancelled_work_is_discarded() {
        let mut times = vec![time(0); 1000];
        times.push(time(1));
        assert_eq!(chart(times, 2).bars, ["█ ", "█▁"]);
        assert!(
            calculate(
                &Request {
                    revision: 0,
                    times: Arc::new(vec![time(0)]),
                    width: 2,
                    series: Arc::default(),
                },
                &AtomicU64::new(1),
            )
            .is_none()
        );
    }

    #[test]
    fn worker_caches_unchanged_data_and_invalidates_on_resize_and_clear() {
        let mut graph = ActivityGraph::default();
        let times = vec![time(0), time(10)];
        graph.set_data(times.clone());
        graph.poll(10);
        let revision = graph.revision.load(Ordering::Relaxed);
        if graph.chart.is_none() {
            let (finished, chart) = graph
                .worker
                .as_ref()
                .unwrap()
                .rx
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert_eq!(finished, revision);
            graph.chart = Some(chart);
            graph.pending = false;
        }
        let cached = graph.chart.as_ref().unwrap().bars[0].as_ptr();
        graph.set_data(times);
        for _ in 0..100 {
            graph.poll(10);
        }
        assert_eq!(graph.revision.load(Ordering::Relaxed), revision);
        assert_eq!(graph.chart.as_ref().unwrap().bars[0].as_ptr(), cached);
        assert!(!graph.pending());
        graph.poll(20);
        assert!(graph.revision.load(Ordering::Relaxed) > revision);
        // A fast worker may already have delivered the resized chart in poll.
        assert!(
            graph.pending()
                || graph
                    .chart
                    .as_ref()
                    .is_some_and(|chart| chart.bars[0].len() == 20)
        );
        graph.set_data(Vec::new());
        assert!(!graph.pending());
        assert!(graph.chart.is_none());
        // A queued result for the previous width must never restore old data.
        graph.poll(20);
        assert!(graph.chart.is_none());
    }

    #[test]
    fn view_snapshot_counts_records_across_hosts_and_continuation_only_matches() {
        let row = |start, seconds| ViewRow {
            line: Line::default(),
            timestamp: Some(time(seconds)),
            error: false,
            start,
            search_match: false,
            truncation_marker: false,
        };
        let source = |record| {
            Some(RowSource {
                key: Arc::from("file"),
                record,
                line: 0,
                first: None,
                raw_range: None,
                wrapped: false,
            })
        };
        let mut logs = LogViewer::default();
        logs.host_views = vec![
            HostView {
                host: "a".into(),
                keys: vec!["a".into()],
                rows: vec![
                    row(true, 0),
                    row(false, 0),
                    row(true, 0),
                    row(false, 0),
                    row(true, 10),
                ],
                matches: vec![1, 2, 3],
                row_sources: vec![source(0), source(0), source(1), source(1), source(2)],
                time_index: Vec::new(),
            },
            HostView {
                host: "b".into(),
                keys: vec!["b".into()],
                rows: vec![row(true, 5)],
                matches: vec![0],
                row_sources: vec![source(0)],
                time_index: Vec::new(),
            },
        ];
        logs.refresh_activity_graph();
        assert_eq!(
            logs.activity_graph.times.as_slice(),
            &[time(0), time(0), time(5)]
        );
        logs.host_views.clear();
        logs.rows = vec![row(false, 0), row(true, 1), row(false, 1), row(false, 2)];
        logs.rows[3].truncation_marker = true;
        logs.matches = vec![0, 1, 2, 3];
        logs.refresh_activity_graph();
        assert_eq!(logs.activity_graph.times.as_slice(), &[time(1)]);
    }
}
