//! Stable chronological merge shared with the terminal viewer.
use super::presentation::{Kind, Stream};
use chrono::NaiveDateTime;
use std::{cmp::Reverse, collections::BinaryHeap};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimeWindow {
    pub start: Option<NaiveDateTime>,
    pub end: Option<NaiveDateTime>,
}
impl TimeWindow {
    pub fn active(self) -> bool {
        self.start.is_some() || self.end.is_some()
    }
    pub fn contains(self, time: Option<NaiveDateTime>) -> bool {
        if !self.active() {
            return true;
        }
        time.is_some_and(|time| {
            self.start.is_none_or(|start| time >= start) && self.end.is_none_or(|end| time <= end)
        })
    }
}

/// One source's cursor. In-order streams need no per-record merge index.
struct MergeSource<'a> {
    stream: &'a Stream,
    ordered: std::ops::Range<usize>,
    unordered: Vec<usize>,
    cursor: usize,
    window: TimeWindow,
    include_untimed: bool,
    kinds: [bool; 4],
}

impl MergeSource<'_> {
    fn includes(&self, index: usize) -> bool {
        let record = &self.stream.records[index];
        (self.window.contains(record.timestamp)
            || (self.include_untimed && record.timestamp.is_none()))
            && self.kinds[match record.kind {
                Kind::Error => 0,
                Kind::Request => 1,
                Kind::Access => 2,
                Kind::Other => 3,
            }]
    }

    fn next_index(&mut self) -> Option<usize> {
        if !self.unordered.is_empty() {
            let index = *self.unordered.get(self.cursor)?;
            self.cursor += 1;
            return Some(index);
        }
        while let Some(index) = self.ordered.next() {
            if self.includes(index) {
                return Some(index);
            }
        }
        None
    }
}

/// Stable chronological merge. The heap holds at most one record per source;
/// only streams containing late/out-of-order records need a sorted local index.
pub struct MergedRecords<'a> {
    sources: Vec<MergeSource<'a>>,
    heap: BinaryHeap<Reverse<(Option<NaiveDateTime>, usize, usize)>>,
    first_time: Option<NaiveDateTime>,
}

impl<'a> MergedRecords<'a> {
    pub fn new(
        streams: &[&'a Stream],
        window: TimeWindow,
        include_untimed: bool,
        kinds: [bool; 4],
    ) -> Self {
        let mut sources = Vec::with_capacity(streams.len());
        let mut heap = BinaryHeap::new();
        let mut first_time: Option<NaiveDateTime> = None;
        for (source, &stream) in streams.iter().enumerate() {
            let range = stream.window_range(window.start, window.end);
            let mut cursor = MergeSource {
                stream,
                ordered: if stream.is_ordered() {
                    range.clone()
                } else {
                    0..0
                },
                unordered: Vec::new(),
                cursor: 0,
                window,
                include_untimed,
                kinds,
            };
            if !stream.is_ordered() {
                let indices = range.filter(|&index| cursor.includes(index)).collect();
                cursor.unordered = indices;
                cursor
                    .unordered
                    .sort_by_key(|&index| stream.records[index].timestamp);
            }
            if let Some(index) = cursor.next_index() {
                let first = if stream.is_ordered() {
                    stream.records[index].timestamp
                } else {
                    cursor
                        .unordered
                        .iter()
                        .find_map(|&index| stream.records[index].timestamp)
                };
                if let Some(first) = first {
                    first_time = Some(first_time.map_or(first, |old| old.min(first)));
                }
                heap.push(Reverse((stream.records[index].timestamp, source, index)));
            }
            sources.push(cursor);
        }
        Self {
            sources,
            heap,
            first_time,
        }
    }

    pub fn first_timestamp(&self) -> Option<NaiveDateTime> {
        self.first_time
    }
}

impl Iterator for MergedRecords<'_> {
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((_, source, index)) = self.heap.pop()?;
        if let Some(next) = self.sources[source].next_index() {
            self.heap.push(Reverse((
                self.sources[source].stream.records[next].timestamp,
                source,
                next,
            )));
        }
        Some((source, index))
    }
}
