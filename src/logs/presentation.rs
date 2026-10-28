//! Native record parsing and compact display, adapted from tmp/pp-pretty-oneline.js.
//! Source text stays intact in the cache; formatting never changes collected logs.
use super::LogFile;
use chrono::{DateTime, FixedOffset, NaiveDateTime};
use quick_xml::{Reader, events::Event};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Error,
    Request,
    Access,
    Other,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Error => "ERR",
            Self::Request => "REQ",
            Self::Access => "ACC",
            Self::Other => "???",
        }
    }
    fn color(self) -> Color {
        match self {
            Self::Error => Color::LightRed,
            Self::Request => Color::LightCyan,
            Self::Access => Color::LightGreen,
            Self::Other => Color::White,
        }
    }
}

pub(crate) fn source(name: &str) -> (Kind, String) {
    let name = name.strip_suffix(".txt").unwrap_or(name);
    let name = name
        .rsplit_once('_')
        .filter(|(_, n)| n.bytes().all(|b| b.is_ascii_digit()))
        .map_or(name, |(name, _)| name);
    let (port, log) = name.split_once('_').unwrap_or(("----", name));
    let port = if port == "TaskServer" { "TASK" } else { port };
    let kind = match log.to_ascii_lowercase().as_str() {
        "errorlog" => Kind::Error,
        "requestlog" => Kind::Request,
        "accesslog" => Kind::Access,
        _ => Kind::Other,
    };
    (kind, port.into())
}

#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub host: String,
    pub file: String,
    pub kind: Kind,
    pub port: String,
    pub timestamp: Option<NaiveDateTime>, // server-local time, including explicit zone conversion
    pub level: String,
    pub raw: String,
    message: String,
    request: Option<Value>,
}

pub(crate) struct Stream {
    pub records: Vec<Record>,
    last_start: usize,
    len: usize,
    ordered: bool,
}
impl Default for Stream {
    fn default() -> Self {
        Self {
            records: Vec::new(),
            last_start: 0,
            len: 0,
            ordered: true,
        }
    }
}
impl Stream {
    /// Use binary search when the stream has monotonically increasing known
    /// timestamps. Unordered or untimestamped streams fall back to scanning.
    pub fn window_range(
        &self,
        start: Option<NaiveDateTime>,
        end: Option<NaiveDateTime>,
    ) -> std::ops::Range<usize> {
        if !self.ordered || (start.is_none() && end.is_none()) {
            return 0..self.records.len();
        }
        let first = start.map_or(0, |start| {
            self.records
                .partition_point(|r| r.timestamp.unwrap() < start)
        });
        let last = end.map_or(self.records.len(), |end| {
            self.records
                .partition_point(|r| r.timestamp.unwrap() <= end)
        });
        first.min(last)..last
    }

    /// Reparse only the final record and appended bytes. This handles partial
    /// lines, Info+/Error+ continuations spanning polls, and a late XML close.
    pub fn update(&mut self, file: &LogFile, text: &str, reset: bool, offset: FixedOffset) {
        if reset || text.len() < self.len {
            *self = Self::default();
        }
        if text.len() == self.len {
            return;
        }
        let start = self.last_start.min(text.len());
        let stable_count = self.records.len().saturating_sub(1);
        if !self.records.is_empty() {
            self.records.pop();
        }
        let mut position = start;
        for line in text[start..].split_inclusive('\n') {
            let raw = line.trim_end_matches(['\r', '\n']);
            if raw.starts_with("==>") || raw.trim().is_empty() {
                position += line.len();
                continue;
            }
            let (mut record, continuation) = parse(file, raw, offset);
            let attach = continuation
                || (record.kind == Kind::Error
                    && record.timestamp.is_none()
                    && !raw.starts_with("-- gap:"));
            if attach && let Some(previous) = self.records.last_mut() {
                previous.raw.push('\n');
                previous.raw.push_str(raw);
                previous.message.push('\n');
                previous.message.push_str(&record.message);
            } else {
                if record.timestamp.is_none() {
                    record.timestamp = self.records.last().and_then(|r| r.timestamp);
                }
                self.last_start = position;
                self.records.push(record);
            }
            position += line.len();
        }
        if self.ordered {
            let tail = &self.records[stable_count.saturating_sub(1).min(self.records.len())..];
            self.ordered = tail.iter().all(|record| record.timestamp.is_some())
                && tail
                    .windows(2)
                    .all(|pair| pair[0].timestamp <= pair[1].timestamp);
        }
        self.len = text.len();
    }
}

static ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d{2}-\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?)\s+(\w+)([:+]*)\s*(.*)$")
        .unwrap()
});
static ACCESS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"^(\S+)\s+.*?\[([^\]]+)\]\s+"([^\"]*)"\s+(\d{3}|-)\s+(\S+)"#).unwrap()
});

fn parse(file: &LogFile, raw: &str, offset: FixedOffset) -> (Record, bool) {
    let (kind, port) = source(&file.name);
    let mut record = Record {
        host: file.host.clone(),
        file: file.name.clone(),
        kind,
        port,
        timestamp: None,
        level: String::new(),
        raw: raw.into(),
        message: raw.into(),
        request: None,
    };
    let mut continuation = false;
    match kind {
        Kind::Error => {
            if let Some(c) = ERROR.captures(raw) {
                record.timestamp = parse_time(&c[1], offset);
                record.level = c[2].into();
                record.message = c[4].into();
                continuation = c[3].contains('+');
            }
        }
        Kind::Request => {
            if let Ok(value) = serde_json::from_str::<Value>(raw) {
                record.timestamp = value
                    .get("time")
                    .and_then(Value::as_str)
                    .and_then(|s| parse_time(s, offset));
                record.request = Some(value);
            }
        }
        Kind::Access => {
            if let Some(c) = ACCESS.captures(raw) {
                record.timestamp = DateTime::parse_from_str(&c[2], "%d/%b/%Y:%H:%M:%S %z")
                    .ok()
                    .map(|t| t.with_timezone(&offset).naive_local());
            }
        }
        Kind::Other => {}
    }
    (record, continuation)
}

/// Timestamp in a raw cached log line, normalized to server-local time.
pub(crate) fn line_time(file: &LogFile, line: &str, offset: FixedOffset) -> Option<NaiveDateTime> {
    parse(file, line, offset).0.timestamp
}

/// Whether a raw line begins a new message. Error-log continuations can have
/// either a timestamped `Error+` prefix or no timestamp at all; access and
/// request records are line-oriented.
pub(crate) fn line_starts_message(file: &LogFile, line: &str, offset: FixedOffset) -> bool {
    let (kind, _) = source(&file.name);
    let (record, continuation) = parse(file, line, offset);
    !(continuation
        || (kind == Kind::Error && record.timestamp.is_none() && !line.starts_with("-- gap:")))
}

pub(crate) fn parse_time(input: &str, offset: FixedOffset) -> Option<NaiveDateTime> {
    DateTime::parse_from_rfc3339(input)
        .ok()
        .map(|t| t.with_timezone(&offset).naive_local())
        .or_else(|| {
            NaiveDateTime::parse_from_str(&input.replace('T', " "), "%Y-%m-%d %H:%M:%S%.f").ok()
        })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum SearchMode {
    Off,
    Highlight,
    #[default]
    Filter,
}
impl SearchMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Highlight => "highlight",
            Self::Filter => "filter",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Options {
    pub raw: bool,
    pub relative: bool,
    pub full_errors: bool,
    pub wrap: bool,
    pub search_mode: SearchMode,
    pub kinds: [bool; 4],
}
impl Default for Options {
    fn default() -> Self {
        Self {
            raw: false,
            relative: false,
            full_errors: false,
            wrap: false,
            search_mode: SearchMode::Filter,
            kinds: [true; 4],
        }
    }
}

pub(crate) struct Patterns {
    expressions: Vec<Regex>,
}
impl Patterns {
    /// Same OR-list convention as the script: literals or /regex/i separated
    /// by |. Separators inside a regex and escaped separators stay intact.
    pub fn new(input: &str) -> Self {
        let mut parts = Vec::new();
        let mut part = String::new();
        let mut in_regex = false;
        let mut escaped = false;
        for ch in input.chars() {
            if escaped {
                part.push(ch);
                escaped = false;
                continue;
            }
            if ch == '\\' {
                part.push(ch);
                escaped = true;
                continue;
            }
            if ch == '/' && (in_regex || part.is_empty()) {
                in_regex = !in_regex;
            }
            if ch == '|' && !in_regex {
                parts.push(std::mem::take(&mut part));
            } else {
                part.push(ch);
            }
        }
        parts.push(part);
        Self {
            expressions: parts
                .into_iter()
                .filter(|s| !s.trim().is_empty())
                .filter_map(|s| {
                    let s = s.trim();
                    let expression = if s.starts_with('/')
                        && let Some(end) = s.rfind('/').filter(|&i| i > 0)
                    {
                        let flags = &s[end + 1..];
                        let flag_prefix = if flags.contains('i') { "(?i)" } else { "" };
                        format!("{flag_prefix}{}", &s[1..end])
                    } else {
                        format!("(?i){}", regex::escape(&s.replace(r"\|", "|")))
                    };
                    Regex::new(&expression)
                        .ok()
                        .or_else(|| Regex::new(&format!("(?i){}", regex::escape(s))).ok())
                })
                .collect(),
        }
    }
    pub fn matches(&self, text: &str) -> bool {
        self.expressions.is_empty() || self.expressions.iter().any(|r| r.is_match(text))
    }
    pub fn highlight(&self, line: Line<'static>) -> Line<'static> {
        let palette = [
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
        ];
        let text = line.to_string();
        let mut ranges: Vec<_> = self
            .expressions
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                r.find_iter(&text)
                    .filter(|m| m.start() < m.end())
                    .map(move |m| (m.start(), m.end(), palette[i % palette.len()]))
            })
            .collect();
        ranges.sort_by_key(|&(start, end, _)| (start, std::cmp::Reverse(end)));
        let mut result = Vec::new();
        let mut offset = 0;
        for span in line.spans {
            let end = offset + span.content.len();
            let mut cursor = offset;
            for &(start, finish, color) in &ranges {
                let start = start.max(cursor);
                let finish = finish.min(end);
                if start >= finish {
                    continue;
                }
                if cursor < start {
                    result.push(Span::styled(
                        span.content[cursor - offset..start - offset].to_string(),
                        span.style,
                    ));
                }
                result.push(Span::styled(
                    span.content[start - offset..finish - offset].to_string(),
                    span.style
                        .bg(color)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ));
                cursor = finish;
            }
            if cursor < end {
                result.push(Span::styled(
                    span.content[cursor - offset..].to_string(),
                    span.style,
                ));
            }
            offset = end;
        }
        Line::from(result)
    }
}

fn colored(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}
pub(crate) fn wrap_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    use unicode_width::UnicodeWidthChar;
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let mut text = String::new();
        for ch in span.content.chars() {
            let cells = ch.width().unwrap_or(0);
            if used + cells > width && used > 0 {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                lines.push(Line::from(std::mem::take(&mut spans)));
                used = 0;
            }
            text.push(ch);
            used += cells;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    if !spans.is_empty() || lines.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}
pub(crate) fn port_color(port: &str) -> Color {
    if port == "----" {
        return Color::White;
    }
    if port == "TASK" {
        return Color::LightMagenta;
    }
    let palette = [
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::LightRed,
        Color::LightGreen,
    ];
    let hash = port
        .bytes()
        .fold(0u16, |h, ch| h.wrapping_mul(31).wrapping_add(u16::from(ch)));
    palette[usize::from(hash) % palette.len()]
}
fn decode(token: &str) -> String {
    let token = token.replace('+', " ");
    urlencoding::decode(&token)
        .map(|v| v.into_owned())
        .unwrap_or(token)
}
fn path_spans(path: &str) -> Vec<Span<'static>> {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    let mut spans = Vec::new();
    for (i, token) in path.split('/').enumerate() {
        if i > 0 {
            spans.push(colored("/", Color::White));
        }
        spans.push(colored(decode(token), Color::LightCyan));
    }
    for (i, field) in query.split('&').filter(|s| !s.is_empty()).enumerate() {
        spans.push(colored(if i == 0 { "?" } else { "&" }, Color::White));
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        spans.push(colored(decode(key), Color::Yellow));
        if field.contains('=') {
            spans.push(colored("=", Color::White));
            spans.push(Span::raw(decode(value)));
        }
    }
    spans
}
fn severity(level: &str) -> (&str, Color) {
    match level {
        "Info" => ("II", Color::White),
        "Error" => ("EE", Color::LightRed),
        "Fatal" => ("FF", Color::LightRed),
        "Alert" => ("AA", Color::LightRed),
        "Warning" => ("WW", Color::Yellow),
        "Notice" => ("NN", Color::LightBlue),
        "Debug" => ("DD", Color::White),
        "" => ("", Color::White),
        other => (other, Color::White),
    }
}

impl Record {
    pub fn is_error(&self) -> bool {
        matches!(
            self.level.as_str(),
            "Error" | "Fatal" | "Alert" | "Critical"
        ) || self.message.contains("<error:error")
    }
    /// Count display rows without creating styled spans for ordinary records.
    /// XML errors can expand independently of their input lines, so they need
    /// the same parser as rendering to determine their exact height.
    pub fn display_line_count(&self, options: &Options) -> usize {
        if options.raw {
            return self.raw.lines().count();
        }
        if self.kind == Kind::Error
            && (self.message.contains("<error:error") || self.message.contains("<error "))
        {
            if let Some(parts) = xml_error(&self.message, options.full_errors) {
                return parts.len().max(1);
            }
        }
        if self.kind == Kind::Access {
            return 1;
        }
        if let Some(request) = &self.request {
            if let Some(obj) = request.as_object() {
                let mut groups = [false; 4];
                for key in obj.keys() {
                    if matches!(key.as_str(), "time" | "user" | "url") {
                        continue;
                    }
                    let group = if key.ends_with("Time") {
                        2
                    } else if key.ends_with("Hits") {
                        0
                    } else if key.ends_with("Misses") {
                        1
                    } else {
                        3
                    };
                    groups[group] = true;
                }
                return 1 + groups.into_iter().filter(|v| *v).count();
            }
            return 1;
        }
        self.message.lines().count().max(1)
    }

    pub fn render(
        &self,
        options: &Options,
        first: Option<NaiveDateTime>,
        hosts: bool,
    ) -> Vec<Line<'static>> {
        if options.raw {
            return self
                .raw
                .lines()
                .map(|line| {
                    Line::from(vec![
                        colored(format!("[{} / {}] ", self.host, self.file), Color::White),
                        Span::raw(line.to_string()),
                    ])
                })
                .collect();
        }
        let time = self
            .timestamp
            .map(|t| {
                if options.relative
                    && let Some(first) = first
                {
                    let ms = (t - first).num_milliseconds();
                    format!(
                        "{}{:02}:{:02}",
                        if ms < 0 { '-' } else { '+' },
                        ms.abs() / 60_000,
                        ms.abs() / 1000 % 60
                    )
                } else {
                    t.format("%H:%M:%S%.3f").to_string()
                }
            })
            .unwrap_or_else(|| "--:--:--.---".into());
        let mut prefix = vec![
            colored(self.kind.label(), self.kind.color()),
            Span::raw(" "),
            colored(format!("{:>5} ", self.port), port_color(&self.port)),
            colored(format!("{time} "), Color::White),
        ];
        if hosts {
            prefix.push(colored(
                format!("{} ", self.host.split('.').next().unwrap_or(&self.host)),
                Color::LightBlue,
            ));
        }
        let indent = " ".repeat(prefix.iter().map(|s| s.content.chars().count()).sum());
        let mut parts = if self.kind == Kind::Access {
            self.access_parts()
        } else if let Some(request) = &self.request {
            request_parts(request)
        } else {
            self.message
                .lines()
                .map(|s| vec![Span::raw(s.to_string())])
                .collect()
        };
        if self.kind == Kind::Error
            && let Some(error) = xml_error(&self.message, options.full_errors)
        {
            parts = error;
        }
        let (level, color) = severity(&self.level);
        if !level.is_empty() {
            prefix.push(colored(format!("{level}: "), color));
        }
        if parts.is_empty() {
            parts.push(vec![Span::raw(self.message.clone())]);
        }
        parts
            .into_iter()
            .enumerate()
            .map(|(i, mut spans)| {
                let mut p = if i == 0 {
                    prefix.clone()
                } else {
                    vec![Span::raw(indent.clone())]
                };
                p.append(&mut spans);
                Line::from(p)
            })
            .collect()
    }
    fn access_parts(&self) -> Vec<Vec<Span<'static>>> {
        let Some(c) = ACCESS.captures(&self.raw) else {
            return vec![vec![Span::raw(self.message.clone())]];
        };
        let mut request = c[3].split_whitespace();
        let method = request.next().unwrap_or("-");
        let path = request.next().unwrap_or("-");
        let status_color = match c[4].bytes().next() {
            Some(b'2') => Color::Green,
            Some(b'3') => Color::Blue,
            Some(b'4') => Color::Yellow,
            Some(b'5') => Color::Red,
            _ => Color::White,
        };
        let mut spans = vec![
            colored(format!("{} ", &c[1]), Color::White),
            colored(format!("{method:<4} "), Color::White),
            colored(format!("{} ", &c[4]), status_color),
            colored(format!("{} ", &c[5]), Color::White),
        ];
        spans.extend(path_spans(path));
        vec![spans]
    }
}

fn value_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}
fn fixed_time(value: &Value) -> String {
    if let Some(v) = value.as_f64() {
        for decimals in [2, 1, 0] {
            let s = format!("{v:.decimals$}");
            if s.len() <= 5 {
                return s;
            }
        }
    }
    value_text(value)
}
fn request_parts(value: &Value) -> Vec<Vec<Span<'static>>> {
    let Some(obj) = value.as_object() else {
        return vec![vec![Span::raw(value.to_string())]];
    };
    let mut first = vec![Span::raw(format!(
        "{} ",
        value.get("user").and_then(Value::as_str).unwrap_or("-")
    ))];
    first.extend(path_spans(
        value.get("url").and_then(Value::as_str).unwrap_or("-"),
    ));
    let mut parts = vec![first];
    for group in ["Hits", "Misses", "Time", "Other"] {
        let mut fields = Vec::new();
        for (key, value) in obj {
            if matches!(key.as_str(), "time" | "user" | "url") {
                continue;
            }
            let actual = if key.ends_with("Time") {
                "Time"
            } else if key.ends_with("Hits") {
                "Hits"
            } else if key.ends_with("Misses") {
                "Misses"
            } else {
                "Other"
            };
            if actual != group {
                continue;
            }
            let label = key
                .strip_suffix(group)
                .unwrap_or(key)
                .trim_end_matches("Cache");
            if !fields.is_empty() {
                fields.push(colored("|", Color::White));
            }
            fields.push(colored(label, Color::Yellow));
            fields.push(colored(":", Color::White));
            fields.push(Span::raw(if group == "Time" {
                fixed_time(value)
            } else {
                value_text(value)
            }));
        }
        if !fields.is_empty() {
            let mut spans = vec![colored(group, Color::LightCyan), colored("[", Color::White)];
            spans.append(&mut fields);
            spans.push(colored("]", Color::White));
            parts.push(spans);
        }
    }
    parts
}

#[derive(Default)]
struct XmlNode {
    name: String,
    text: String,
    children: Vec<XmlNode>,
}
impl XmlNode {
    fn child(&self, name: &str) -> Option<&Self> {
        self.children.iter().find(|n| n.name == name)
    }
    fn text(&self, name: &str) -> &str {
        self.child(name).map_or("", |n| n.text.trim())
    }
}
fn unescape(text: &str) -> String {
    let mut text = text.to_string();
    for _ in 0..5 {
        let next = quick_xml::escape::unescape(&text)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| text.clone());
        if next == text {
            break;
        }
        text = next;
    }
    text
}
fn xml_error(text: &str, full: bool) -> Option<Vec<Vec<Span<'static>>>> {
    let start = text.find("<error:error").or_else(|| text.find("<error "))?;
    let end = text[start..]
        .find("</error:error>")
        .map(|i| start + i + 14)
        .or_else(|| text[start..].find("</error>").map(|i| start + i + 8))?;
    let mut reader = Reader::from_str(&text[start..end]);
    let mut stack = Vec::<XmlNode>::new();
    let mut root = None;
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) => stack.push(XmlNode {
                name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                ..XmlNode::default()
            }),
            Event::Empty(e) => {
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(XmlNode {
                        name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                        ..XmlNode::default()
                    });
                }
            }
            Event::Text(e) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&e.decode().ok()?);
                }
            }
            Event::CData(e) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push_str(&e.decode().ok()?);
                }
            }
            Event::GeneralRef(e) => {
                if let Some(node) = stack.last_mut() {
                    node.text.push('&');
                    node.text.push_str(&e.decode().ok()?);
                    node.text.push(';');
                }
            }
            Event::End(_) => {
                let mut node = stack.pop()?;
                node.text = unescape(&node.text);
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    root = Some(node);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let error = root?;
    let mut lines = Vec::new();
    let mut heading = vec![colored(error.text("code"), Color::LightRed)];
    if !error.text("name").is_empty() {
        heading.push(colored(
            format!(" ({})", error.text("name")),
            Color::LightYellow,
        ));
    }
    if !error.text("xquery-version").is_empty() {
        heading.push(colored(
            format!(" {}", error.text("xquery-version")),
            Color::LightMagenta,
        ));
    }
    lines.push(heading);
    let summary = if error.text("format-string").is_empty() {
        error.text("message")
    } else {
        error.text("format-string")
    };
    if !summary.is_empty() {
        lines.push(vec![
            colored("summary: ", Color::White),
            Span::raw(summary.to_string()),
        ]);
    }
    for key in ["expr", "retryable"] {
        if !error.text(key).is_empty() {
            lines.push(vec![
                colored(format!("{key}: "), Color::White),
                Span::raw(error.text(key).to_string()),
            ]);
        }
    }
    if let Some(frames) = error.child("stack") {
        for (i, frame) in frames
            .children
            .iter()
            .filter(|n| n.name == "frame")
            .enumerate()
        {
            lines.push(vec![
                colored(format!("frame {} ", i + 1), Color::White),
                colored(frame.text("uri"), Color::LightCyan),
                colored(
                    format!(":{}:{}", frame.text("line"), frame.text("column")),
                    Color::LightMagenta,
                ),
            ]);
            if full {
                for key in ["operation", "xquery-version"] {
                    if !frame.text(key).is_empty() {
                        lines.push(vec![
                            colored(format!("  {key}: "), Color::White),
                            Span::raw(frame.text(key).to_string()),
                        ]);
                    }
                }
                if let Some(vars) = frame.child("variables") {
                    for var in &vars.children {
                        lines.push(vec![
                            colored(format!("  ${}: ", var.text("name")), Color::LightGreen),
                            Span::raw(var.text("value").to_string()),
                        ]);
                    }
                }
            }
        }
    }
    // Preserve non-XML context that the script discarded.
    for context in [text[..start].trim(), text[end..].trim()] {
        if !context.is_empty() {
            lines.push(vec![Span::raw(context.to_string())]);
        }
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(name: &str) -> LogFile {
        LogFile {
            host: "host-one.example.com".into(),
            path: format!("/Logs/{name}"),
            name: name.into(),
            size: 100,
            modified: "2026-10-14T00:00:00Z".into(),
            present: true,
        }
    }
    fn plain(record: &Record, options: &Options) -> String {
        record
            .render(options, None, false)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn window_ranges_are_inclusive_and_fall_back_for_unordered_streams() {
        let offset = FixedOffset::east_opt(0).unwrap();
        let mut stream = Stream::default();
        let lines = "2026-10-14 08:00:00.000 Info: first\n2026-10-14 08:05:00.000 Info: second\n2026-10-14 08:05:00.000 Info: duplicate\n2026-10-14 08:10:00.000 Info: last\n";
        stream.update(&file("ErrorLog.txt"), lines, false, offset);
        let point = parse_time("2026-10-14 08:05:00", offset).unwrap();
        assert_eq!(stream.window_range(Some(point), Some(point)), 1..3);
        let append = format!("{lines}2026-10-14 08:03:00.000 Info: out of order\n");
        stream.update(&file("ErrorLog.txt"), &append, false, offset);
        assert_eq!(stream.window_range(Some(point), Some(point)), 0..5);
        let mut untimed = Stream::default();
        untimed.update(
            &file("ErrorLog.txt"),
            "unknown preface\n2026-10-14 08:05:00.000 Info: known\n",
            false,
            offset,
        );
        assert_eq!(untimed.window_range(Some(point), Some(point)), 0..2);
    }

    #[test]
    fn row_counts_match_rendering_for_log_formats_and_options() {
        let offset = FixedOffset::east_opt(0).unwrap();
        let cases = [
            (
                "ErrorLog.txt",
                "2026-10-14 12:30:01.123 Error: failure\n2026-10-14 12:30:01.123 Error+: details\n",
            ),
            (
                "ErrorLog.txt",
                "2026-10-14 12:30:01.123 Error: <error:error><error:code>FAIL</error:code><error:message>bad</error:message></error:error>\n",
            ),
            (
                "9060_AccessLog.txt",
                "127.0.0.1 - admin [14/Oct/2026:10:30:01 +0000] \"GET /v1/search HTTP/1.1\" 200 89\n",
            ),
            (
                "9060_RequestLog.txt",
                "{\"time\":\"2026-10-14T10:30:01Z\",\"url\":\"/query\",\"elapsedTime\":1.2,\"listCacheHits\":2,\"missMisses\":3}\n",
            ),
        ];
        for (name, text) in cases {
            let mut stream = Stream::default();
            stream.update(&file(name), text, false, offset);
            for record in &stream.records {
                for raw in [false, true] {
                    for full_errors in [false, true] {
                        let options = Options {
                            raw,
                            full_errors,
                            ..Options::default()
                        };
                        assert_eq!(
                            record.display_line_count(&options),
                            record.render(&options, None, false).len(),
                            "{name}: {text}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn late_continuations_and_partial_lines_update_the_last_record() {
        let offset = FixedOffset::east_opt(7200).unwrap();
        let mut stream = Stream::default();
        let first = "2026-10-14 12:30:01.123 Error: failure";
        stream.update(&file("9060_ErrorLog.txt"), first, false, offset);
        let extended = format!(
            "{first}\n2026-10-14 12:30:01.123 Error+: stack frame\n2026-10-14 12:30:02.000 Info: done\n"
        );
        stream.update(&file("9060_ErrorLog.txt"), &extended, false, offset);
        assert_eq!(stream.records.len(), 2);
        assert!(stream.records[0].raw.contains("Error+: stack frame"));
        let text = plain(&stream.records[0], &Options::default());
        assert!(text.starts_with("ERR  9060 12:30:01.123 EE: failure"));
        assert!(text.contains("stack frame"));
        stream.update(
            &file("9060_ErrorLog.txt"),
            "2026-10-14 12:31:00.000 Info: rotated\n",
            true,
            offset,
        );
        assert_eq!(stream.records.len(), 1);
        assert!(stream.records[0].raw.contains("rotated"));
    }
    #[test]
    fn access_and_request_timestamps_share_server_time_and_compact_fields() {
        let offset = FixedOffset::east_opt(7200).unwrap();
        let mut access = Stream::default();
        access.update(&file("9060_AccessLog_1.txt"), "127.0.0.1 - admin [14/Oct/2026:10:30:01 +0000] \"GET /v1/search?query=some%20text HTTP/1.1\" 500 89 \"-\" \"agent\"\n", false, offset);
        let mut request = Stream::default();
        request.update(&file("9060_RequestLog.txt"), "{\"time\":\"2026-10-14T10:30:01Z\",\"user\":\"admin\",\"url\":\"/query?a=b\",\"elapsedTime\":1.234,\"listCacheHits\":2,\"listCacheMisses\":3}\n", false, offset);
        assert_eq!(access.records[0].timestamp, request.records[0].timestamp);
        assert_eq!(access.records[0].port, "9060");
        let text = plain(&access.records[0], &Options::default());
        assert!(text.contains("12:30:01.000"));
        assert!(text.contains("GET  500 89 /v1/search?query=some text"));
        assert!(!text.contains("agent"));
        let text = plain(&request.records[0], &Options::default());
        assert!(text.contains("Hits[list:2]"));
        assert!(text.contains("Misses[list:3]"));
        assert!(text.contains("Time[elapsed:1.23]"));
    }
    #[test]
    fn xml_errors_decode_entities_and_keep_full_stack_variables_available() {
        let text = r#"context <error:error xmlns:error="http://marklogic.com/xdmp/error"><error:code>XDMP-FAIL</error:code><error:message>a &amp;lt; b</error:message><error:stack><error:frame><error:uri>/module.xqy</error:uri><error:line>42</error:line><error:column>3</error:column><error:operation>do-work()</error:operation><error:variables><error:variable><error:name>value</error:name><error:value>secret detail</error:value></error:variable></error:variables></error:frame></error:stack></error:error>"#;
        let compact = xml_error(text, false)
            .unwrap()
            .into_iter()
            .map(|s| Line::from(s).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(compact.contains("summary: a < b"));
        assert!(compact.contains("/module.xqy:42:3"));
        assert!(!compact.contains("secret detail"));
        let full = xml_error(text, true)
            .unwrap()
            .into_iter()
            .map(|s| Line::from(s).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full.contains("$value: secret detail"));
        assert!(full.contains("operation: do-work()"));
        assert!(xml_error("<error:error><bad>", false).is_none());
    }
    #[test]
    fn regex_alternation_or_literals_and_unicode_wrapping_work() {
        let patterns = Patterns::new("/error|warning/i|duration");
        assert!(patterns.matches("WARNING"));
        assert!(patterns.matches("duration"));
        assert!(!patterns.matches("info"));
        let line = patterns.highlight(Line::from("warning durée"));
        assert!(line.spans.iter().any(|s| s.style.bg.is_some()));
        let wrapped = wrap_line(Line::from("你好世界abcdef"), 5);
        assert!(wrapped.iter().all(|l| l.width() <= 5));
        assert_eq!(
            wrapped.iter().map(Line::to_string).collect::<String>(),
            "你好世界abcdef"
        );
    }
}
